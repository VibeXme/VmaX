// =======
// Copyright (c) 2017-2019 Advanced Micro Devices, Inc. All rights reserved.
// -------
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation
// files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy,
// modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
// -------
// The above copyright notice and this permission notice shall be included in all copies or substantial portions of the
// Software.
// -------
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE
// WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL THE AUTHORS OR
// COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
// ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//------------------------------------------------------------------------------------------------------------------------------

// FidelityFX CAS sharpen-only filter adapted for the native viewer's RGB target.
Texture2D<float4> frame : register(t0);
cbuffer CasParams : register(b0) {
    int2 max_pixel;
    float peak;
    float padding;
};

float4 fullscreen(uint vertex : SV_VertexID) : SV_POSITION {
    float2 pos = float2((vertex << 1) & 2, vertex & 2);
    return float4(pos * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}

float3 load_pixel(int2 pos) {
    float3 rgb = frame.Load(int3(clamp(pos, int2(0, 0), max_pixel), 0)).rgb;
    // BT.709 video processor output is nonlinear. Filter in linear light.
    return rgb * rgb;
}

float4 sharpen(float4 position : SV_POSITION) : SV_TARGET {
    int2 p = int2(position.xy);
    if (peak == 0.0) return frame.Load(int3(p, 0));
    float3 center = load_pixel(p);
    float3 north = load_pixel(p + int2(0, -1));
    float3 south = load_pixel(p + int2(0,  1));
    float3 west  = load_pixel(p + int2(-1, 0));
    float3 east  = load_pixel(p + int2( 1, 0));
    float low = min(center.g, min(min(north.g, south.g), min(west.g, east.g)));
    float high = max(center.g, max(max(north.g, south.g), max(west.g, east.g)));
    float amplitude = sqrt(saturate(min(low, 1.0 - high) / max(high, 1e-6)));
    float weight = amplitude * peak;
    float3 rgb = saturate((center + (north + south + west + east) * weight) / (1.0 + 4.0 * weight));
    return float4(sqrt(rgb), 1.0);
}
