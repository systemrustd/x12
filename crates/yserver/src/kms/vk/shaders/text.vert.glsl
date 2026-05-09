#version 450

// Per-glyph quad vertex shader (sub-phase 4.1.4.5). Driven by
// gl_VertexIndex from a 4-vertex `vkCmdDraw(4, 1, ...)` per glyph
// with TRIANGLE_STRIP topology — no vertex buffer is bound. Push
// constants carry this glyph's destination rect (in mirror pixel
// coords), the mirror viewport, and the source UV rect into the
// shared atlas.
//
// NDC convention matches `composite.vert.glsl`: y increases
// downward; pixel `(0, 0)` lands at NDC `(-1, -1)` (top-left).

layout(push_constant) uniform PushConsts {
    vec2 dst_origin;
    vec2 dst_size;
    vec2 viewport;
    vec2 src_origin;   // [0..1] of atlas
    vec2 src_size;
    vec4 foreground;   // RGB used by fragment shader; alpha is 1.0
} pc;

layout(location = 0) out vec2 v_uv;
layout(location = 1) out vec4 v_foreground;

void main() {
    vec2 quad = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));

    vec2 dst_pixel = pc.dst_origin + quad * pc.dst_size;
    vec2 ndc = dst_pixel / pc.viewport * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);

    v_uv = pc.src_origin + quad * pc.src_size;
    v_foreground = pc.foreground;
}
