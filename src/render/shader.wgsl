// hyprlaser fragment shader.
//
// Draws a Google-Slides / Excalidraw-style laser pointer: a small red
// head dot with a thin solid red streak trailing behind it. The streak
// is a polyline through the recent cursor positions, with both
// thickness and opacity tapering smoothly to zero at the tail.
//
// History buffer:
//   trail[0]      — newest sample (this frame's cursor, head of the streak)
//   trail[len-1]  — oldest live sample (tail tip)
//   sample.z      — normalised age in [0, 1] (0 = fresh, 1 = about to be dropped)
//
// Per-segment thickness and opacity interpolate from the head sample's
// values down to the tail sample's values. We accumulate by taking the
// maximum coverage across all segments at each fragment — using max
// rather than sum keeps the apparent brightness independent of how
// densely-packed the samples happen to be.
//
// Performance note: with a fullscreen surface (e.g. 5120x1440 = 7.4M
// pixels) and 32 trail segments, a naive evaluation would do ~240M
// segment-distance computations per frame. The vast majority of those
// pixels are nowhere near the streak, so we cull aggressively:
//
//   1. A coarse AABB around the whole trail (computed CPU-side and
//      passed as a uniform). Pixels outside that box only do the cheap
//      head-dot test and bail.
//   2. A per-segment AABB rejection: skip the segment_distance call
//      entirely if the fragment is outside the segment's bounds plus
//      a small padding for the half-width.
//
// Output uses premultiplied alpha because the wgpu surface is
// configured with CompositeAlphaMode::PreMultiplied.

const TRAIL_SAMPLES: u32 = 32u;

struct Uniforms {
    resolution: vec2<f32>,
    // Head dot radius in pixels. Also the maximum half-width of the
    // streak right at the head.
    dot_radius: f32,
    // Half-pixel anti-aliasing band on every edge.
    edge_softness: f32,
    // Half-width at the tail (oldest sample). Should be small, e.g. 0.5.
    tail_half_width: f32,
    _pad: f32,
    // Number of valid trail entries.
    trail_len: u32,
    _pad2: u32,
    // Linear-RGB laser color. Alpha component is unused.
    color: vec4<f32>,
    // Axis-aligned bounding box around the entire trail polyline,
    // expanded by `dot_radius + edge_softness`. Pixels outside this
    // box cannot possibly be on the streak and skip the per-segment
    // loop entirely — this is the dominant optimisation.
    trail_bounds_min: vec2<f32>,
    trail_bounds_max: vec2<f32>,
    // (x, y, age, _unused) per entry.
    trail: array<vec4<f32>, TRAIL_SAMPLES>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> @builtin(position) vec4<f32> {
    let px = f32((vid << 1u) & 2u) * 2.0 - 1.0;
    let py = f32(vid & 2u) * 2.0 - 1.0;
    return vec4<f32>(px, py, 0.0, 1.0);
}

// Distance from point `p` to the line segment AB, along with the
// parametric `t` of the closest point (0 at A, 1 at B). Used so we can
// linearly interpolate per-vertex attributes along the segment.
fn segment_distance(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> vec2<f32> {
    let ab = b - a;
    let ap = p - a;
    let len_sq = max(dot(ab, ab), 1e-6);
    let t = clamp(dot(ap, ab) / len_sq, 0.0, 1.0);
    let proj = a + t * ab;
    return vec2<f32>(distance(p, proj), t);
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let head = u.trail[0].xy;

    // Hard core dot at the head. Cheap test — just a distance check.
    let d_head = distance(frag.xy, head);
    let core_alpha = 1.0 - smoothstep(
        u.dot_radius - u.edge_softness,
        u.dot_radius + u.edge_softness,
        d_head,
    );

    // Coarse AABB reject. The trail bounds already include the
    // dot_radius + edge_softness padding, so any pixel outside cannot
    // contribute to the streak. ~99% of fullscreen pixels short-circuit
    // here.
    let p = frag.xy;
    let outside =
        p.x < u.trail_bounds_min.x ||
        p.y < u.trail_bounds_min.y ||
        p.x > u.trail_bounds_max.x ||
        p.y > u.trail_bounds_max.y;

    var streak_alpha = 0.0;
    if (!outside && u.trail_len >= 2u) {
        let last = u.trail_len - 1u;
        let pad = u.dot_radius + u.edge_softness;
        for (var i: u32 = 0u; i < last; i = i + 1u) {
            let a_xy = u.trail[i].xy;
            let b_xy = u.trail[i + 1u].xy;

            // Per-segment AABB reject. We pad by the head's half-width
            // (the maximum half-width of the streak) so we never falsely
            // reject pixels near a thicker segment.
            let seg_min = min(a_xy, b_xy) - vec2<f32>(pad);
            let seg_max = max(a_xy, b_xy) + vec2<f32>(pad);
            if (p.x < seg_min.x || p.y < seg_min.y ||
                p.x > seg_max.x || p.y > seg_max.y) {
                continue;
            }

            let a_age = u.trail[i].z;
            let b_age = u.trail[i + 1u].z;

            let dt = segment_distance(p, a_xy, b_xy);
            let dist = dt.x;
            let t = dt.y;

            // Interpolated age at the closest point on this segment.
            let age = mix(a_age, b_age, t);

            // Per-pixel half-width: at the head (age 0) the streak is
            // dot_radius wide; at the tail (age 1) it shrinks to
            // tail_half_width. Squared falloff makes the visible thickness
            // taper a bit more dramatically, matching the Google Slides
            // / Excalidraw reference.
            let one_minus_age = 1.0 - age;
            let taper = one_minus_age * one_minus_age;
            let half_width = mix(u.tail_half_width, u.dot_radius, taper);

            // Cheap distance reject: if we're outside this segment's
            // half-width + AA band, the smoothstep result is 0 anyway.
            if (dist > half_width + u.edge_softness) {
                continue;
            }

            // Anti-aliased edge.
            let edge = 1.0 - smoothstep(
                half_width - u.edge_softness,
                half_width + u.edge_softness,
                dist,
            );

            // Opacity along the streak: full opacity near the head,
            // fading smoothly to zero at the tail. (1 - age)^1.5 gives a
            // slow fade in the middle and a quicker drop-off at the very
            // tip — matches the reference screenshots better than a
            // linear or quadratic curve. We compute it as x * sqrt(x)
            // instead of pow(x, 1.5) which is meaningfully cheaper on
            // most GPUs.
            let opacity = one_minus_age * sqrt(one_minus_age);

            streak_alpha = max(streak_alpha, edge * opacity);
        }
    }

    // Combine head core with streak. The (1 - core_alpha) factor stops
    // the streak from pushing the already-saturated centre over 1.0.
    let alpha = clamp(core_alpha + streak_alpha * (1.0 - core_alpha), 0.0, 1.0);

    return vec4<f32>(u.color.rgb * alpha, alpha);
}
