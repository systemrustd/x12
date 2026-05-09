#version 450

// Fragment shader for the composite pass: samples the bound
// texture (one per draw — the source window or pixmap mirror), with
// optional alpha pass-through driven by a per-quad push-constant
// flag.
//
// `use_src_alpha == 1.0` → keep sampled alpha (cursor + alpha
// pixmaps blend src-over against existing scanout content).
// `use_src_alpha == 0.0` → force alpha to 1.0 (opaque windows
// where the pixman X8R8G8B8 mirror's alpha pad isn't guaranteed to
// be 0xFF).
//
// The pipeline blend state is src-over either way; the alpha
// override is what differentiates an opaque window from a
// shape-masked / cursor draw.

layout(location = 0) in vec2 v_uv;
layout(location = 1) in float v_use_alpha;

layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D tex;

void main() {
    vec4 c = texture(tex, v_uv);
    out_color = (v_use_alpha > 0.5) ? c : vec4(c.rgb, 1.0);
}
