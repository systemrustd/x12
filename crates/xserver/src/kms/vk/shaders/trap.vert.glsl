#version 450

// GPU rasterization for RENDER Trapezoids (gpu-trap T1). Emits a
// unit quad (4 vertices via TRIANGLE_STRIP) covering the per-draw
// bbox, one quad per instance. Per-instance attributes encode the
// trapezoid's geometry; they are flat-interpolated to the fragment
// stage which computes analytic coverage.

layout(push_constant) uniform PushConsts {
    vec2 mask_extent;        // mask scratch image extent (pixels)
    vec2 bbox_origin_pixel;  // top-left of bbox in ABSOLUTE pixel coords
    vec2 bbox_size_pixel;    // bbox size in pixels
    vec2 _pad;
} pc;

// Per-instance trapezoid attributes (stride = 40, INSTANCE rate).
layout(location = 0) in float in_top;
layout(location = 1) in float in_bottom;
layout(location = 2) in vec2 in_left_p1;
layout(location = 3) in vec2 in_left_p2;
layout(location = 4) in vec2 in_right_p1;
layout(location = 5) in vec2 in_right_p2;

layout(location = 0) flat out float top;
layout(location = 1) flat out float bottom;
layout(location = 2) flat out vec2 left_p1;
layout(location = 3) flat out vec2 left_p2;
layout(location = 4) flat out vec2 right_p1;
layout(location = 5) flat out vec2 right_p2;

void main() {
    // Unit-quad index pattern: (0,0), (1,0), (0,1), (1,1) for
    // TRIANGLE_STRIP. The vertex shader is invoked 4 times per
    // instance (gl_VertexIndex in [0..4)) and emits the four
    // corners of the bbox in NDC.
    //
    // gpu-trap T2: the quad emits at MaskScratch-LOCAL coords
    // (0..bbox_w, 0..bbox_h), not absolute mask coords. This puts
    // the GPU-rasterized mask data at MaskScratch[0..bbox_w,
    // 0..bbox_h], matching the pre-gpu-trap CPU-upload convention;
    // the surrounding composite path then samples
    // mask[(dst.x + mask_origin.x)] without changes.
    // The fragment shader still computes coverage in ABSOLUTE coords
    // (trap edges arrive in absolute pixel coords from the X
    // protocol) — it adds `bbox_origin_pixel` to `gl_FragCoord` to
    // recover that.
    vec2 quad = vec2(float(gl_VertexIndex & 1),
                     float((gl_VertexIndex >> 1) & 1));
    vec2 pixel = quad * pc.bbox_size_pixel;
    vec2 ndc = pixel / pc.mask_extent * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);

    top = in_top;
    bottom = in_bottom;
    left_p1 = in_left_p1;
    left_p2 = in_left_p2;
    right_p1 = in_right_p1;
    right_p2 = in_right_p2;
}
