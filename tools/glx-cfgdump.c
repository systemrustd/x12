/* glx-cfgdump.c — dump every driConfigEqual/attribMap attribute for the
 * fbconfigs that have an X visual at depth 24 or 32. Run against a
 * radeonsi display (Xwayland :0) to learn the exact attribute values a
 * server must advertise for mesa's GLX dri3 screen creation to match.
 *
 * gcc tools/glx-cfgdump.c -lGL -lX11 -o ./cfgdump
 * XAUTHORITY=... DISPLAY=:0 ./cfgdump
 */
#include <GL/glx.h>
#include <X11/Xlib.h>
#include <stdio.h>

static int a(Display *d, GLXFBConfig c, int attr) {
    int v = -1;
    return glXGetFBConfigAttrib(d, c, attr, &v) == Success ? v : -999;
}

int main(void) {
    Display *d = XOpenDisplay(NULL);
    if (!d) { fprintf(stderr, "cannot open display\n"); return 1; }
    int screen = DefaultScreen(d);
    int n = 0;
    GLXFBConfig *cfgs = glXGetFBConfigs(d, screen, &n);
    printf("%d configs total; depth-24/32 visual-bound ones:\n", n);
    for (int i = 0; i < n; i++) {
        GLXFBConfig c = cfgs[i];
        XVisualInfo *vi = glXGetVisualFromFBConfig(d, c);
        int vdepth = vi ? vi->depth : -1;
        if (vi) XFree(vi);
        if (vdepth != 24 && vdepth != 32) continue;
        printf("fbid=0x%x vis=0x%x vdepth=%d | bufsz=%d lvl=%d "
               "r=%d g=%d b=%d al=%d dp=%d st=%d "
               "ar=%d ag=%d ab=%d aa=%d sb=%d samp=%d "
               "db=%d stereo=%d aux=%d "
               "rtype=0x%x btRGB=%d btRGBA=%d btMip=%d btTargets=0x%x yinv=%d srgb=%d\n",
               a(d,c,GLX_FBCONFIG_ID), a(d,c,GLX_VISUAL_ID), vdepth,
               a(d,c,GLX_BUFFER_SIZE), a(d,c,GLX_LEVEL),
               a(d,c,GLX_RED_SIZE), a(d,c,GLX_GREEN_SIZE), a(d,c,GLX_BLUE_SIZE), a(d,c,GLX_ALPHA_SIZE),
               a(d,c,GLX_DEPTH_SIZE), a(d,c,GLX_STENCIL_SIZE),
               a(d,c,GLX_ACCUM_RED_SIZE), a(d,c,GLX_ACCUM_GREEN_SIZE),
               a(d,c,GLX_ACCUM_BLUE_SIZE), a(d,c,GLX_ACCUM_ALPHA_SIZE),
               a(d,c,GLX_SAMPLE_BUFFERS), a(d,c,GLX_SAMPLES),
               a(d,c,GLX_DOUBLEBUFFER), a(d,c,GLX_STEREO), a(d,c,GLX_AUX_BUFFERS),
               a(d,c,GLX_RENDER_TYPE),
               a(d,c,GLX_BIND_TO_TEXTURE_RGB_EXT), a(d,c,GLX_BIND_TO_TEXTURE_RGBA_EXT),
               a(d,c,GLX_BIND_TO_MIPMAP_TEXTURE_EXT), a(d,c,GLX_BIND_TO_TEXTURE_TARGETS_EXT),
               a(d,c,GLX_Y_INVERTED_EXT), a(d,c,GLX_FRAMEBUFFER_SRGB_CAPABLE_EXT));
    }
    return 0;
}
