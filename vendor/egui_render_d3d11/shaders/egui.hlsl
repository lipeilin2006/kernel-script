// egui D3D11 painter shaders (vs_4_0 / ps_4_0, FL10_0+).
// Semantics mirror egui_render_glow: vertices in egui logical points,
// top-left origin, premultiplied alpha passthrough (no srgb conversions:
// RGBA8 texture + straight-through shader == glow's srgb-texture +
// srgb-output-fs net result).

cbuffer Constants : register(b0)
{
    float2 screen_size;
    float2 _pad0;
};

struct VSIn
{
    float2 pos : POSITION;
    float2 uv  : TEXCOORD0;
    float4 col : COLOR0;
};

struct VSOut
{
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
    float4 col : COLOR0;
};

Texture2D tex0 : register(t0);
SamplerState samp0 : register(s0);

VSOut vs_main(VSIn i)
{
    VSOut o;
    float2 ndc = float2(i.pos.x / screen_size.x * 2.0 - 1.0,
                        1.0 - i.pos.y / screen_size.y * 2.0);
    o.pos = float4(ndc, 0.0, 1.0);
    o.uv = i.uv;
    o.col = i.col;
    return o;
}

float4 ps_main(VSOut i) : SV_Target
{
    return i.col * tex0.Sample(samp0, i.uv);
}
