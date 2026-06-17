#version 450

// Glyph fragment shader (sub-phase 4.1.4.5). Samples the shared
// R8 glyph atlas (alpha-only) and emits the GC foreground colour
// modulated by the sampled alpha — i.e. the standard X11
// `Operation::Over` glyph composite where the source is a 1×1
// solid-fill of `foreground` and the mask is the glyph bitmap.
//
// Output is premultiplied: `(rgb * α, α)`. The pipeline's blend
// state then performs `dst_rgb = src_rgb + dst_rgb * (1 - src_a)`
// against the mirror, matching the pixman path's behaviour.

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_foreground;

layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D atlas;

void main() {
    float alpha = texture(atlas, v_uv).r;
    out_color = vec4(v_foreground.rgb * alpha, alpha);
}
