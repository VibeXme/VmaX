// build.rs - Compile HLSL shaders at build time using dxc
// and generate platform-specific Tauri context.
//
// This script compiles the HLSL shaders to DXIL bytecode (.cso files)
// and places them in OUT_DIR for the runtime to load.
//
// Requirements:
// - dxc (DirectX Shader Compiler) must be in PATH
//   - Windows: Install "DirectX Shader Compiler" from Visual Studio Installer
//   - Or download from https://github.com/microsoft/DirectXShaderCompiler/releases

use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::fs;

fn main() {
    tauri_build::build();

    // Tauri attaches its Windows resource object (icon + manifest with the
    // Common-Controls v6 dependency) to bin targets via `rustc-link-arg-bins`,
    // which cargo only applies to bin targets. Test binaries would otherwise
    // fall back to the mingw CRT `default-manifest.o`, whose manifest lacks
    // Common-Controls: comctl32 then resolves to v5.82, which does not export
    // `TaskDialogIndirect`, and the test process dies at load with
    // STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139). Link the same resource object
    // into every final link unit — it is a plain COFF object, so the linker
    // includes it fully, and placed before the CRT endfile its manifest wins.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        if let Ok(out_dir) = env::var("OUT_DIR") {
            let resource = PathBuf::from(&out_dir).join("libresource.a");
            if resource.exists() {
                println!("cargo:rustc-link-arg={}", resource.display());
            }
        }
    }

    // Generate platform-specific Tauri context to avoid Windows-only icon loading macro

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let shader_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("shaders");
    
    // Tell cargo to re-run if shader files change
    println!("cargo:rerun-if-changed=shaders/nv12_cas_presentation.hlsl");
    println!("cargo:rerun-if-changed=src/native_client.rs"); // for inline shaders
    
    // Compile vertex shader (inline in native_client.rs)
    compile_inline_shader("vs_main", "vs_5_0", &out_dir);
    
    // Compile video pixel shader
    compile_shader_file(&shader_dir.join("nv12_cas_presentation.hlsl"), "main", "ps_5_0", "ps_video", &out_dir);
    
    // Compile cursor pixel shader (inline in native_client.rs)
    compile_inline_shader("ps_cursor", "ps_5_0", &out_dir);
}

fn compile_inline_shader(name: &str, target: &str, out_dir: &PathBuf) {
    // Extract inline shader source from native_client.rs
    // For simplicity, we compile a minimal version here
    let source = match name {
        "vs_main" => r#"
            struct VS_IN { float2 pos : POSITION; float2 uv : TEXCOORD0; };
            struct VS_OUT { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
            VS_OUT main(VS_IN input) {
                VS_OUT output;
                output.pos = float4(input.pos, 0.0, 1.0);
                output.uv = input.uv;
                return output;
            }
        "#,
        "ps_cursor" => r#"
            Texture2D<float4> tex_cursor : register(t0);
            SamplerState samp : register(s0);
            
            cbuffer CursorParams : register(b0) {
                float2 cursor_pos;
                float2 cursor_scale;
                float2 hotspot;
                float2 _pad;
            };
            
            float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
                float2 local_uv = (uv - cursor_pos + hotspot * cursor_scale) / cursor_scale;
                if (local_uv.x < 0.0 || local_uv.x > 1.0 || local_uv.y < 0.0 || local_uv.y > 1.0) {
                    return float4(0, 0, 0, 0);
                }
                return tex_cursor.Sample(samp, local_uv);
            }
        "#,
        _ => return,
    };
    
    compile_shader_source(source, "main", target, name, out_dir);
}

fn compile_shader_file(file: &PathBuf, entry: &str, target: &str, name: &str, out_dir: &PathBuf) {
    let source = fs::read_to_string(file).unwrap_or_else(|_| {
        eprintln!("Warning: Could not read shader file {:?}", file);
        String::new()
    });
    compile_shader_source(&source, entry, target, name, out_dir);
}

fn compile_shader_source(source: &str, entry: &str, target: &str, name: &str, out_dir: &PathBuf) {
    let output_path = out_dir.join(format!("{}.cso", name));
    
    // Try to find dxc
    let dxc_path = find_dxc();
    if dxc_path.is_none() {
        eprintln!("Warning: dxc not found in PATH, skipping shader compilation for {}", name);
        eprintln!("  Install DirectX Shader Compiler from Visual Studio Installer or https://github.com/microsoft/DirectXShaderCompiler/releases");
        return;
    }
    
    let dxc = dxc_path.unwrap();
    
    let mut cmd = Command::new(&dxc);
    cmd.args([
        "-T", target,
        "-E", entry,
        "-Fo", output_path.to_str().unwrap(),
        "-Zi",           // Debug info
        "-Qstrip_reflect", // Strip reflection data
        "-Wall",         // All warnings
        "-WX",           // Warnings as errors
    ]);
    
    // Write source to temp file for dxc
    let temp_file = out_dir.join(format!("{}.hlsl", name));
    fs::write(&temp_file, source).unwrap();
    cmd.arg(temp_file.to_str().unwrap());
    
    println!("Compiling shader {} with dxc...", name);
    let status = cmd.status();
    
    match status {
        Ok(s) if s.success() => {
            println!("  Successfully compiled {} to {:?}", name, output_path);
        }
        Ok(s) => {
            eprintln!("  Failed to compile {}: dxc exited with {}", name, s);
        }
        Err(e) => {
            eprintln!("  Failed to run dxc for {}: {}", name, e);
        }
    }
}

fn find_dxc() -> Option<PathBuf> {
    // Check common locations
    let candidates = [
        "dxc.exe",
        "dxc",
        "C:\\Program Files (x86)\\Windows Kits\\10\\bin\\10.0.22621.0\\x64\\dxc.exe",
        "C:\\Program Files\\Microsoft DirectX Shader Compiler\\bin\\dxc.exe",
    ];
    
    for candidate in &candidates {
        if which(candidate).is_some() {
            return Some(PathBuf::from(candidate));
        }
    }
    
    None
}

fn which(cmd: &str) -> Option<PathBuf> {
    if let Ok(path) = env::var("PATH") {
        for dir in path.split(';') {
            let full = PathBuf::from(dir).join(cmd);
            if full.exists() {
                return Some(full);
            }
        }
    }
    None
}