// Single-pass NV12 -> RGB + Scaling + FidelityFX CAS Presentation Shader
//
// This shader combines:
// 1. BT.709 Full-Range (0-255) NV12 -> RGB conversion
// 2. Bilinear scaling to viewport resolution (decode_size -> output_size)
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
//   float2 inv_y_tex_size;      // 1/Y_width, 1/Y_height
//   float2 inv_uv_tex_size;     // 1/UV_width, 1/UV_height
//   float  sharpness;           // 0.0 to 1.0 (default 0.45)
//   float  full_range;          // 1.0 = Full Range (0-255), 0.0 = Studio (16-235)
//   float2 decode_size;         // Decoded frame size (Y plane)
//   float2 output_size;         // Output/swap chain size
//   float2 uv_scale;            // decode_size / output_size
//   float2 _pad;

Texture2D<float>  tex_y  : register(t0);
Texture2D<float2> tex_uv : register(t1);
SamplerState      samp   : register(s0);

cbuffer CasParams : register(b0) {
    float2 inv_y_tex_size;
    float2 inv_uv_tex_size;
    float  sharpness;
    float  full_range;
    float2 decode_size;
    float2 output_size;
    float2 uv_scale;
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
    // Scale UV from output space to decode space
    float2 decode_uv = uv * uv_scale;
    
    // Sample at the exact output pixel center for perfect scaling
    float3 c = sample_bt709_rgb(decode_uv);
    
    if (sharpness <= 0.01) {
        return float4(c, 1.0);
    }

    // CAS: Sample 4-tap cross neighborhood in decode UV space
    // Y plane steps
    float2 inv_y_x = float2(inv_y_tex_size.x, 0.0);
    float2 inv_y_y = float2(0.0, inv_y_tex_size.y);
    // UV plane steps (half resolution)
    float2 inv_uv_x = float2(inv_uv_tex_size.x, 0.0);
    float2 inv_uv_y = float2(0.0, inv_uv_tex_size.y);
    
    // Sample neighborhoods - use correct step sizes per plane
    // For Y plane sampling:
    float3 y_n = sample_bt709_rgb(decode_uv - inv_y_y);
    float3 y_s = sample_bt709_rgb(decode_uv + inv_y_y);
    float3 y_w = sample_bt709_rgb(decode_uv - inv_y_x);
    float3 y_e = sample_bt709_rgb(decode_uv + inv_y_x);
    
    // For UV plane sampling (sample_bt709_rgb samples both planes):
    // The UV coordinates passed are for Y plane; UV plane is sampled at half resolution automatically
    // due to texture dimensions. But for neighborhood we need UV-plane-relative steps.
    // Since sample_bt709_rgb uses the same UV for both, and UV texture is half-size,
    // a step of inv_y_y in Y space = 2*inv_uv_y in UV space.
    // We need separate UV-plane steps.
    float3 uv_n = sample_bt709_rgb(decode_uv - inv_uv_y);
    float3 uv_s = sample_bt709_rgb(decode_uv + inv_uv_y);
    float3 uv_w = sample_bt709_rgb(decode_uv - inv_uv_x);
    float3 uv_e = sample_bt709_rgb(decode_uv + inv_uv_x);

    // Contrast adaptive weights - combine both plane neighborhoods
    // Use Y plane for luma contrast, UV plane for chroma
    float3 min_rgb = min(c, min(min(y_n, y_s), min(y_w, y_e)));
    float3 max_rgb = max(c, max(max(y_n, y_s), max(y_w, y_e)));
    float3 amp = saturate(min(min_rgb, 1.0 - max_rgb) / max(max_rgb, 1e-4));
    float3 weight = -sqrt(amp) * lerp(0.125, 0.20, saturate(sharpness));

    // Sharpened output using Y plane neighborhood (more accurate for luma)
    float3 out_rgb = saturate((c + (y_n + y_s + y_w + y_e) * weight) / (1.0 + 4.0 * weight));
    return float4(out_rgb, 1.0);
}