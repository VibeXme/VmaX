// Single-pass NV12 -> RGB + Scaling + FidelityFX CAS Presentation Shader
// 
// This shader combines:
// 1. BT.709 Full-Range (0-255) NV12 -> RGB conversion
// 2. Bilinear scaling to viewport resolution
// 3. AMD FidelityFX Contrast Adaptive Sharpening (CAS)
// All in a single pixel shader pass for minimum latency.
//
// Registers:
//   t0: Y plane (R8_UNORM)
//   t1: UV plane (R8G8_UNORM)
//   s0: Linear clamp sampler
//   b0: CasParams constant buffer
//
// Constant buffer layout (CasParams):
//   float2 inv_tex_size;        // 1/Y_width, 1/UV_width
//   float  sharpness;           // 0.0 to 1.0 (default 0.45)
//   float  full_range;          // 1.0 = Full Range (0-255), 0.0 = Studio (16-235)
//   float2 decode_size;         // Decoded frame size (Y plane)
//   float2 output_size;         // Output/swap chain size
//   float2 _pad;

Texture2D<float>  tex_y  : register(t0);
Texture2D<float2> tex_uv : register(t1);
SamplerState      samp   : register(s0);

cbuffer CasParams : register(b0) {
    float2 inv_tex_size;
    float  sharpness;
    float  full_range;
    float2 decode_size;
    float2 output_size;
    float2 _pad;
};

float3 sample_bt709_rgb(float2 uv) {
    float y_raw  = tex_y.SampleLevel(samp, uv, 0);
    float2 cbcr  = tex_uv.SampleLevel(samp, uv, 0) - float2(0.5, 0.5);
    
    // Full Range (0-255) vs Studio Range (16-235/240)
    float y = (full_range > 0.5) 
        ? y_raw 
        : saturate((y_raw - (16.0 / 255.0)) * (255.0 / 219.0));
    float2 c = (full_range > 0.5) 
        ? cbcr 
        : cbcr * (255.0 / 224.0);

    // Exact BT.709 Full Range matrix
    float r = y + 1.5748 * c.y;
    float g = y - 0.1873 * c.x - 0.4681 * c.y;
    float b = y + 1.8556 * c.x;
    return saturate(float3(r, g, b));
}

float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    // Sample at the exact output pixel center for perfect scaling
    float3 c = sample_bt709_rgb(uv);
    
    if (sharpness <= 0.01) {
        return float4(c, 1.0);
    }

    // CAS: Sample 4-tap cross neighborhood in UV space
    float2 inv_y = float2(inv_tex_size.x, 0.0);
    float2 inv_uv = float2(0.0, inv_tex_size.y);
    
    float3 n = sample_bt709_rgb(uv - inv_uv);
    float3 s = sample_bt709_rgb(uv + inv_uv);
    float3 w = sample_bt709_rgb(uv - inv_y);
    float3 e = sample_bt709_rgb(uv + inv_y);

    // Contrast adaptive weights
    float3 min_rgb = min(c, min(min(n, s), min(w, e)));
    float3 max_rgb = max(c, max(max(n, s), max(w, e)));
    float3 amp = saturate(min(min_rgb, 1.0 - max_rgb) / max(max_rgb, 1e-4));
    float3 weight = -sqrt(amp) * lerp(0.125, 0.20, saturate(sharpness));

    // Sharpened output
    float3 out_rgb = saturate((c + (n + s + w + e) * weight) / (1.0 + 4.0 * weight));
    return float4(out_rgb, 1.0);
}