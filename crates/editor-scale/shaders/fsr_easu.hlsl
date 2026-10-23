// SoundCraft: EASU pass of AMD FidelityFX FSR 1 (MIT, see LICENSE-FidelityFX-FSR.txt).
// The two #include lines are replaced by the vendored headers before compilation (win.rs).
// 8x8 threads, each covering a 2x2 block: one group produces a 16x16 output tile.
#define A_GPU 1
#define A_HLSL 1
#define FSR_EASU_F 1
#include "ffx_a.h"
#include "ffx_fsr1.h"

Texture2D<float4> InputTexture : register(t0);
SamplerState LinearClamp : register(s0);
RWTexture2D<float4> OutputTexture : register(u0);
// xy: input (native) size in pixels, zw: output (scaled) size in pixels.
cbuffer Params : register(b0) { float4 gParams; }

// Input callbacks: normalized texture coordinates in, gathered red/green/blue out.
AF4 FsrEasuRF(AF2 p) { return InputTexture.GatherRed(LinearClamp, p); }
AF4 FsrEasuGF(AF2 p) { return InputTexture.GatherGreen(LinearClamp, p); }
AF4 FsrEasuBF(AF2 p) { return InputTexture.GatherBlue(LinearClamp, p); }

[numthreads(8, 8, 1)]
void main(uint3 gid : SV_GroupID, uint3 gtid : SV_GroupThreadID)
{
    AU4 con0;
    AU4 con1;
    AU4 con2;
    AU4 con3;
    FsrEasuCon(con0, con1, con2, con3, gParams.x, gParams.y, gParams.x, gParams.y, gParams.z, gParams.w);
    uint outW = (uint)gParams.z;
    uint outH = (uint)gParams.w;
    uint2 basePos = gid.xy * 16u + gtid.xy * 2u;
    [unroll] for (uint j = 0; j < 2; j++)
    {
        [unroll] for (uint i = 0; i < 2; i++)
        {
            uint2 ip = basePos + uint2(i, j);
            if (ip.x < outW && ip.y < outH)
            {
                AF3 pix;
                FsrEasuF(pix, ip, con0, con1, con2, con3);
                OutputTexture[ip] = float4(pix, 1.0);
            }
        }
    }
}
