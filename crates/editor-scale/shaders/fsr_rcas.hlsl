// SoundCraft: RCAS pass of AMD FidelityFX FSR 1 (MIT, see LICENSE-FidelityFX-FSR.txt).
// The two #include lines are replaced by the vendored headers before compilation (win.rs).
// Sharpness 0.2 stops, AMD's default. 8x8 threads, each covering a 2x2 block.
#define A_GPU 1
#define A_HLSL 1
#define FSR_RCAS_F 1
#include "ffx_a.h"
#include "ffx_fsr1.h"

Texture2D<float4> InputTexture : register(t0);
RWTexture2D<float4> OutputTexture : register(u0);

// Input callback: integer pixel of the EASU output, clamped to its bounds.
AF4 FsrRcasLoadF(ASU2 p)
{
    uint w;
    uint h;
    InputTexture.GetDimensions(w, h);
    int2 q = clamp(p, int2(0, 0), int2(int(w) - 1, int(h) - 1));
    return InputTexture.Load(int3(q, 0));
}

void FsrRcasInputF(inout AF1 r, inout AF1 g, inout AF1 b) {}

[numthreads(8, 8, 1)]
void main(uint3 gid : SV_GroupID, uint3 gtid : SV_GroupThreadID)
{
    AU4 con;
    FsrRcasCon(con, 0.2);
    uint w;
    uint h;
    InputTexture.GetDimensions(w, h);
    uint2 basePos = gid.xy * 16u + gtid.xy * 2u;
    [unroll] for (uint j = 0; j < 2; j++)
    {
        [unroll] for (uint i = 0; i < 2; i++)
        {
            uint2 ip = basePos + uint2(i, j);
            if (ip.x < w && ip.y < h)
            {
                AF1 r;
                AF1 g;
                AF1 b;
                FsrRcasF(r, g, b, ip, con);
                OutputTexture[ip] = float4(r, g, b, 1.0);
            }
        }
    }
}
