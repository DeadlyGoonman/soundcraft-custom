// SoundCraft: bilinear upscale (one output pixel per thread, 8x8 threads).
// xy: input (native) size in pixels, zw: output (scaled) size in pixels.
Texture2D<float4> InputTexture : register(t0);
SamplerState LinearClamp : register(s0);
RWTexture2D<float4> OutputTexture : register(u0);
cbuffer Params : register(b0) { float4 gParams; }

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= (uint)gParams.z || id.y >= (uint)gParams.w)
    {
        return;
    }
    float2 uv = (float2(id.xy) + 0.5) / gParams.zw;
    OutputTexture[id.xy] = InputTexture.SampleLevel(LinearClamp, uv, 0);
}
