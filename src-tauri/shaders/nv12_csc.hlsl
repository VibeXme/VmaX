// DXGI format views expose logical RGB components. NV12 and P010 use
// separate plane UAVs. Chroma is horizontally left-sited and vertically centred.
Texture2D<float4> Src : register(t0);
RWTexture2D<float> YPlane : register(u0);
RWTexture2D<float2> UVPlane : register(u1);
RWTexture2D<float4> AyuvPlane : register(u2);

static const float3 kLuma = float3(0.2126, 0.7152, 0.0722);
static const float3 kCb = float3(-0.1146, -0.3854, 0.5000);
static const float3 kCr = float3(0.5000, -0.4542, -0.0458);

float3 LoadRgb(int2 p, int2 size) {
    return Src.Load(int3(clamp(p, int2(0, 0), size - 1), 0)).rgb;
}

float2 PackP010(float2 v) {
    // R16 UNORM stores only the upper 10 bits of each P010 word.
    return round(saturate(v) * 1023.0) * (64.0 / 65535.0);
}

void Convert(uint2 p, bool tenBit) {
    uint width, height;
    Src.GetDimensions(width, height);
    if (p.x >= width || p.y >= height) return;

    int2 samplePos = int2(p);
    int2 size = int2(width, height);
    float3 rgb = LoadRgb(samplePos, size);
    float y = saturate(dot(rgb, kLuma));
    YPlane[p] = tenBit ? PackP010(y.xx).x : y;
    if ((p.x & 1) != 0 || (p.y & 1) != 0) return;

    // Average two rows. Applying a half-pixel horizontal shift to the
    // 2x2 box gives left/centre/right weights of 1/4, 1/2, 1/4.
    float3 chromaRgb = (rgb + LoadRgb(samplePos + int2(0, 1), size)) * 0.25;
    chromaRgb += (LoadRgb(samplePos + int2(-1, 0), size)
               +  LoadRgb(samplePos + int2(-1, 1), size)
               +  LoadRgb(samplePos + int2( 1, 0), size)
               +  LoadRgb(samplePos + int2( 1, 1), size)) * 0.125;
    float2 uv = saturate(float2(dot(chromaRgb, kCb), dot(chromaRgb, kCr)) + 0.5);
    UVPlane[p >> 1] = tenBit ? PackP010(uv) : uv;
}

[numthreads(8, 8, 1)]
void nv12(uint3 tid : SV_DispatchThreadID) {
    Convert(tid.xy, false);
}

[numthreads(8, 8, 1)]
void p010(uint3 tid : SV_DispatchThreadID) {
    Convert(tid.xy, true);
}

[numthreads(8, 8, 1)]
void ayuv(uint3 tid : SV_DispatchThreadID) {
    uint width, height;
    Src.GetDimensions(width, height);
    if (tid.x >= width || tid.y >= height) return;

    float3 rgb = Src.Load(int3(tid.xy, 0)).rgb;
    float y = dot(rgb, kLuma);
    float u = dot(rgb, kCb) + 0.5;
    float v = dot(rgb, kCr) + 0.5;
    // AYUV exposes V, U, Y and A in the RGBA8 UAV channels.
    AyuvPlane[tid.xy] = saturate(float4(v, u, y, 1.0));
}
