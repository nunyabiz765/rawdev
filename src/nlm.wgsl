// Non-local means on the GPU: the same integer maths as nlm.rs (and cv2), written as a compute shader.
//
// One workgroup = one 16 x 16 block of output pixels, one thread ("invocation") per pixel.
// The host replaces `CH` with 1 (luma) or 2 (the a/b pair) before compiling; `if (CH == 2u)` is then
// a constant, and the compiler drops the unused branch.
//
// Steps per workgroup:
//   1. copy the 42 x 42 neighbourhood the block needs (16 + 2 * 13 border) from the padded planes in
//      global memory into fast on-chip "workgroup" memory, once. With two channels, both go into one
//      u32 (a in the low 16 bits, b in the high 16), so one read fetches both;
//   2. for each of the 441 search offsets (oy, ox):
//        a. squared differences between the block's 22 x 22 patch area and the same area shifted
//           by (oy, ox)                                      -> diff  (22 x 22)
//        b. vertical 7-sums of diff                          -> vsum  (16 x 22)
//        c. each thread adds 7 vsums = its 7 x 7 patch distance, looks up the weight, and adds
//           weight and weight * candidate value to its own running totals (registers);
//   3. each thread divides by its weight sum (rounding like OpenCV) and writes its pixel.
// `workgroupBarrier()` makes every thread in the block wait until the others reach the same point, so
// nobody reads a shared value before it has been written.

const CH: u32 = __CH__;
const S_HALF: i32 = 10;      // 21 x 21 search window (the patch is 7 x 7)
const PAD: u32 = 13u;        // search half + patch half
const B: u32 = 16u;          // output block size
const SH: u32 = 42u;         // B + 2 * PAD: shared tile side
const DW: u32 = 22u;         // B + 6: patch-area side
const NONE: u32 = 0xffffffffu;

struct Params {
    pw: u32,          // padded plane width
    ph: u32,          // padded plane height
    width: u32,       // output width
    height: u32,      // output height
    plane_words: u32, // u32 words per padded plane (bytes / 4, rounded up)
    cap: u32,         // last index of the weight table (its zero entry)
    row0: u32,        // first output row of this dispatch (band)
    _pad: u32,
}

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> planes: array<u32>;   // padded u8 planes, 4 per word
@group(0) @binding(2) var<storage, read> table: array<u32>;    // weight per (distance >> 6)
@group(0) @binding(3) var<storage, read_write> outbuf: array<u32>; // per pixel: ch0 | ch1 << 8

var<workgroup> tile: array<u32, SH * SH>;
var<workgroup> diff: array<u32, DW * DW>;
var<workgroup> vsum: array<u32, B * DW>;

fn sample(c: u32, x: u32, y: u32) -> u32 {
    let i = y * P.pw + x;
    let word = planes[c * P.plane_words + (i >> 2u)];
    return (word >> ((i & 3u) * 8u)) & 0xffu;
}

// Squared difference of two tile entries, summed over the channels.
fn sqdiff(p: u32, q: u32) -> u32 {
    let t0 = i32(p & 0xffffu) - i32(q & 0xffffu);
    var d = u32(t0 * t0);
    if (CH == 2u) {
        let t1 = i32(p >> 16u) - i32(q >> 16u);
        d += u32(t1 * t1);
    }
    return d;
}

// diff cell k (if it exists) for the shift `off`; `a` is the cell's centre in the tile.
fn diff_cell(k: u32, a: u32, off: i32) {
    if (k != NONE) {
        diff[k] = sqdiff(tile[a], tile[u32(i32(a) + off)]);
    }
}

// vsum cell k (if it exists); `b` is the diff index at the top of its 7-tall column.
fn vsum_cell(k: u32, b: u32) {
    if (k != NONE) {
        vsum[k] = diff[b] + diff[b + DW] + diff[b + 2u * DW] + diff[b + 3u * DW]
            + diff[b + 4u * DW] + diff[b + 5u * DW] + diff[b + 6u * DW];
    }
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_id) lid: vec3<u32>,
        @builtin(local_invocation_index) li: u32) {
    let x0 = wg.x * B;                 // block origin in output pixels
    let y0 = P.row0 + wg.y * B;
    // 1. load the tile. Tile (r, c) = padded (y0 + r, x0 + c); output (y0 + ly, x0 + lx) sits at
    //    tile (ly + PAD, lx + PAD). Reads past the plane edge are clamped; only blocks hanging off the
    //    image edge read them, and those pixels are never written.
    for (var k = li; k < SH * SH; k += 256u) {
        let py = min(y0 + k / SH, P.ph - 1u);
        let px = min(x0 + k % SH, P.pw - 1u);
        var v = sample(0u, px, py);
        if (CH == 2u) {
            v |= sample(1u, px, py) << 16u;
        }
        tile[k] = v;
    }
    workgroupBarrier();

    // Work out once which diff / vsum cells this thread fills (each thread does at most two of each:
    // 484 and 352 cells over 256 threads), so the offset loop has no divisions in it.
    let k0 = li;
    let k1 = li + 256u;
    let dk1 = select(NONE, k1, k1 < DW * DW);
    let da0 = (k0 / DW + 10u) * SH + k0 % DW + 10u;
    let da1 = (k1 / DW + 10u) * SH + k1 % DW + 10u;
    let vk1 = select(NONE, k1, k1 < B * DW);
    let vb0 = k0;   // vsum (r, j) starts at diff (r, j): same index, both rows are DW wide
    let vb1 = k1;
    let vb = lid.y * DW + lid.x;
    let centre = (lid.y + PAD) * SH + lid.x + PAD;

    var wsum = 0u;
    var est0 = 0u;
    var est1 = 0u;
    for (var oy = -S_HALF; oy <= S_HALF; oy++) {
        for (var ox = -S_HALF; ox <= S_HALF; ox++) {
            let off = oy * i32(SH) + ox;   // the shift (oy, ox) as a tile index offset
            // a. diff(i, j) = squared difference between tile(i + 10, j + 10) and the same shifted by
            //    (oy, ox), for the 22 x 22 area whose 7 x 7 windows are the block's patches
            diff_cell(k0, da0, off);
            diff_cell(dk1, da1, off);
            workgroupBarrier();
            // b. vsum(r, j) = diff(r, j) + ... + diff(r + 6, j)
            vsum_cell(k0, vb0);
            vsum_cell(vk1, vb1);
            workgroupBarrier();
            // c. this thread's patch distance -> weight -> accumulate
            let dist = vsum[vb] + vsum[vb + 1u] + vsum[vb + 2u] + vsum[vb + 3u] + vsum[vb + 4u]
                + vsum[vb + 5u] + vsum[vb + 6u];
            let w = table[min(dist >> 6u, P.cap)];
            wsum += w;
            let q = tile[u32(i32(centre) + off)];
            est0 += w * (q & 0xffffu);
            if (CH == 2u) {
                est1 += w * (q >> 16u);
            }
            // (no barrier needed here: the next diff write is followed by a barrier before vsum is
            //  rewritten, and every thread has finished reading vsum before it can pass that barrier)
        }
    }

    // 3. divide, rounding like OpenCV: (sum + wsum / 2) / wsum
    let x = x0 + lid.x;
    let y = y0 + lid.y;
    if (x < P.width && y < P.height) {
        var packed = min((est0 + wsum / 2u) / wsum, 255u);
        if (CH == 2u) {
            packed |= min((est1 + wsum / 2u) / wsum, 255u) << 8u;
        }
        outbuf[y * P.width + x] = packed;
    }
}
