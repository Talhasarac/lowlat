// Colour conversion on D3D11: the duplicated desktop, eight-bit BGRA, into the
// layout an encoder takes, written through one view per plane.
//
// Compiled ahead of time, one variant per layout (scripts/gen-d3d11-convert.sh):
// NV12 and P010, the two-plane 4:2:0 layouts at eight and ten bits; AYUV and
// Y410, the packed full-chroma words at eight and ten. The rules are the Linux
// conversion's: BT.709, limited range with the eight- and ten-bit constants
// kept apart, chroma as the mean of each 2x2 block, the edge repeated rather
// than wrapped, and no dither. Every sample is quantised here and written as
// the exact normalised value of its integer, so the result does not depend on
// how a driver rounds a float into a normalised store; a ten-bit sample of the
// two-plane layout lands in the high ten bits of its sixteen. The views' own
// sizes bound the work, so no constant is needed.

// BT.709, and not selectable: a receiver applies the inverse unconditionally
// with nothing on the wire saying which matrix was used.
static const float KR = 0.2126;
static const float KB = 0.0722;
static const float KG = 1.0 - KR - KB;

// Limited range. **The two sets are not one set rescaled**: 235 of 255 is
// 0.92157 and 940 of 1023 is 0.91887, so carrying the eight-bit numbers into
// a ten-bit target clips white by a fraction of a step at every pixel.
#if defined(P010) || defined(Y410)
static const float TOP = 1023.0;
static const float Y_SCALE = 876.0 / 1023.0;
static const float Y_OFFSET = 64.0 / 1023.0;
static const float C_SCALE = 896.0 / 1023.0;
static const float C_OFFSET = 512.0 / 1023.0;
#else
static const float TOP = 255.0;
static const float Y_SCALE = 219.0 / 255.0;
static const float Y_OFFSET = 16.0 / 255.0;
static const float C_SCALE = 224.0 / 255.0;
static const float C_OFFSET = 128.0 / 255.0;
#endif

// The duplicated image, read through a view of its own format, so the red,
// green and blue below are those channels whatever order memory holds them in.
Texture2D<float4> source : register(t0);

float3 fetch(int2 at)
{
    // Clamped rather than wrapped. An odd width or height leaves one column or
    // row whose partner is off the edge, and repeating the last pixel is the
    // only choice that does not invent colour at the border.
    uint w, h;
    source.GetDimensions(w, h);
    return source.Load(int3(min(at, int2(w, h) - 1), 0)).rgb;
}

float luma_of(float3 c)
{
    return KR * c.r + KG * c.g + KB * c.b;
}

// The sample a normalised value comes to at the target's depth, rounded to the
// nearest and held inside the range.
uint quantise(float v)
{
    return (uint)clamp(v * TOP + 0.5, 0.0, TOP);
}

uint quantise_y(float3 c)
{
    return quantise(luma_of(c) * Y_SCALE + Y_OFFSET);
}

// Blue difference, then red difference, of one colour.
uint2 quantise_c(float3 c)
{
    float y = luma_of(c);
    float u = (c.b - y) / (2.0 - 2.0 * KB);
    float v = (c.r - y) / (2.0 - 2.0 * KR);
    return uint2(quantise(u * C_SCALE + C_OFFSET), quantise(v * C_SCALE + C_OFFSET));
}

#if defined(NV12) || defined(P010)

#if defined(NV12)
// A sample as the eight-bit plane stores it.
float stored(uint q) { return float(q) * (1.0 / 255.0); }
#else
// **A ten-bit sample in the high bits of its sixteen**: 1023 stores as 65472
// and not as 65535, or every value decodes a fraction of a step dark.
float stored(uint q) { return float(q << 6) * (1.0 / 65535.0); }
#endif

RWTexture2D<float> out_y : register(u0);
RWTexture2D<float2> out_uv : register(u1);

// One thread owns a 2x2 block: four luma samples and the one chroma sample
// they share.
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    uint cw, ch;
    out_uv.GetDimensions(cw, ch);
    if (id.x >= cw || id.y >= ch)
        return;
    int2 base = int2(id.xy) * 2;
    float3 c00 = fetch(base);
    float3 c10 = fetch(base + int2(1, 0));
    float3 c01 = fetch(base + int2(0, 1));
    float3 c11 = fetch(base + int2(1, 1));
    out_y[base] = stored(quantise_y(c00));
    out_y[base + int2(1, 0)] = stored(quantise_y(c10));
    out_y[base + int2(0, 1)] = stored(quantise_y(c01));
    out_y[base + int2(1, 1)] = stored(quantise_y(c11));
    // **Average the four colours, then convert once.** The transform is linear
    // in the source, so this equals converting four times and averaging.
    uint2 c = quantise_c((c00 + c10 + c01 + c11) * 0.25);
    out_uv[id.xy] = float2(stored(c.x), stored(c.y));
}

#elif defined(AYUV) || defined(Y410)

// One packed word a pixel: AYUV is V, U, Y, A from the low byte up, which the
// view's red, green, blue and alpha are; Y410 is U, Y, V, A from the low bits
// up at 10:10:10:2, the same order through its own view.
RWTexture2D<float4> out_packed : register(u0);

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    uint w, h;
    out_packed.GetDimensions(w, h);
    if (id.x >= w || id.y >= h)
        return;
    float3 colour = fetch(int2(id.xy));
    uint y = quantise_y(colour);
    uint2 c = quantise_c(colour);
#if defined(AYUV)
    out_packed[id.xy] = float4(float(c.y), float(c.x), float(y), 255.0) * (1.0 / 255.0);
#else
    out_packed[id.xy] = float4(float3(float(c.x), float(y), float(c.y)) * (1.0 / 1023.0), 1.0);
#endif
}

#endif
