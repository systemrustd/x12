#version 450

// GPU rasterization for RENDER Triangles (gpu-trap T3). Emits a
// unit quad (4 vertices via TRIANGLE_STRIP) covering the per-draw
// bbox, one quad per instance. Per-instance attributes encode the
// triangle's three corners; they are flat-interpolated to the
// fragment stage which computes analytic edge coverage.
//
// Winding-order handling: RENDER does NOT specify a winding
// convention — triangles arrive in either CW or CCW order. The
// vertex shader computes the signed-area of (p1, p2, p3) once per
// instance and forwards its sign as `orient` (flat, +1 / -1) so the
// fragment shader can pick a consistent inside-side for each edge
// regardless of winding. This mirrors the sign-agnostic CPU
// reference in `vk/ops/traps.rs::point_in_triangle` (which uses
// "all-three-signs-agree" barycentric tests).

layout(push_constant) uniform PushConsts {
    vec2 mask_extent;        // mask scratch image extent (pixels)
    vec2 bbox_origin_pixel;  // top-left of bbox in ABSOLUTE pixel coords
    vec2 bbox_size_pixel;    // bbox size in pixels
    vec2 _pad;
} pc;

// Per-instance triangle attributes (stride = 24, INSTANCE rate).
layout(location = 0) in vec2 in_p1;
layout(location = 1) in vec2 in_p2;
layout(location = 2) in vec2 in_p3;

layout(location = 0) flat out vec2 p1;
layout(location = 1) flat out vec2 p2;
layout(location = 2) flat out vec2 p3;
// Winding-order sign: +1 for CCW (signed_area_2 >= 0), -1 for CW.
// Used as `inside_side` for all 3 edge_coverage_linear calls in the
// fragment so the half-plane convention matches the triangle's
// actual orientation.
layout(location = 3) flat out float orient;

void main() {
    // Unit-quad index pattern: (0,0), (1,0), (0,1), (1,1) for
    // TRIANGLE_STRIP. The vertex shader is invoked 4 times per
    // instance (gl_VertexIndex in [0..4)) and emits the four
    // corners of the bbox in NDC.
    //
    // Same convention as the trapezoid pipeline: the quad emits at
    // MaskScratch-LOCAL coords (0..bbox_w, 0..bbox_h), not absolute
    // mask coords. The fragment shader translates back to absolute
    // coords by adding `bbox_origin_pixel` to `gl_FragCoord` for the
    // edge math (triangle corners arrive in absolute pixel coords
    // from the X protocol).
    vec2 quad = vec2(float(gl_VertexIndex & 1),
                     float((gl_VertexIndex >> 1) & 1));
    vec2 pixel = quad * pc.bbox_size_pixel;
    vec2 ndc = pixel / pc.mask_extent * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);

    p1 = in_p1;
    p2 = in_p2;
    p3 = in_p3;

    // Signed area × 2 of (p1, p2, p3). Positive ⇒ CCW; negative ⇒ CW;
    // ~0 ⇒ collinear (degenerate). The degenerate case is handled
    // downstream by the `len < 1e-6` guard inside
    // `edge_coverage_linear`, which forces one or more edges to
    // return 0 coverage, collapsing the product to 0. So the sign
    // chosen here for a degenerate triangle is irrelevant.
    float signed_area_2 =
        (in_p2.x - in_p1.x) * (in_p3.y - in_p1.y) -
        (in_p2.y - in_p1.y) * (in_p3.x - in_p1.x);
    orient = signed_area_2 >= 0.0 ? 1.0 : -1.0;
}
