/* glx-tfp-probe.c — replicate muffin's get_fbconfig_for_depth() decision.
 *
 * Build on silence:  gcc tools/glx-tfp-probe.c -lGL -lX11 -o /tmp/tfp-probe
 * Run against yserver: DISPLAY=:7 /tmp/tfp-probe
 *
 * For each FBConfig it prints the exact attributes muffin's
 * get_fbconfig_for_depth (cogl-winsys-glx.c) consults, and whether a
 * usable TFP config exists for depth 24 and depth 32. If "USABLE TFP
 * config for depth N: NO" prints for the window depths, that's why
 * muffin logs "Not using GLX TFP!".
 */
#include <GL/glx.h>
#include <X11/Xlib.h>
#include <stdio.h>
#include <string.h>

static int attr(Display *d, GLXFBConfig c, int a) {
    int v = -1;
    int rc = glXGetFBConfigAttrib(d, c, a, &v);
    return rc == Success ? v : -999; /* -999 = BadAttribute/not returned */
}

int main(void) {
    Display *d = XOpenDisplay(NULL);
    if (!d) { fprintf(stderr, "cannot open display\n"); return 1; }
    int screen = DefaultScreen(d);

    const char *ext = glXQueryExtensionsString(d, screen);
    printf("client GLX ext has texture_from_pixmap: %s\n\n",
           (ext && strstr(ext, "GLX_EXT_texture_from_pixmap")) ? "YES" : "NO");

    int n = 0;
    GLXFBConfig *cfgs = glXGetFBConfigs(d, screen, &n);
    printf("glXGetFBConfigs returned %d configs\n\n", n);

    int usable24 = 0, usable32 = 0;
    for (int i = 0; i < n; i++) {
        GLXFBConfig c = cfgs[i];
        XVisualInfo *vi = glXGetVisualFromFBConfig(d, c);
        int vdepth = vi ? vi->depth : -1;
        int fbid   = attr(d, c, GLX_FBCONFIG_ID);
        int vid    = attr(d, c, GLX_VISUAL_ID);
        int bufsz  = attr(d, c, GLX_BUFFER_SIZE);
        int alpha  = attr(d, c, GLX_ALPHA_SIZE);
        int btr    = attr(d, c, GLX_BIND_TO_TEXTURE_RGB_EXT);
        int btra   = attr(d, c, GLX_BIND_TO_TEXTURE_RGBA_EXT);
        int dtype  = attr(d, c, GLX_DRAWABLE_TYPE);
        int rtype  = attr(d, c, GLX_RENDER_TYPE);
        printf("cfg %d: fbid=0x%x visualID=0x%x visualDepth=%d bufSize=%d alpha=%d "
               "BIND_RGB=%d BIND_RGBA=%d drawableType=0x%x renderType=0x%x\n",
               i, fbid, vid, vdepth, bufsz, alpha, btr, btra, dtype, rtype);
        if (vi) XFree(vi);

        /* muffin get_fbconfig_for_depth, depth=24: needs visualDepth==24,
           (bufSize==24 || bufSize-alpha==24), and BIND_RGB. */
        if (vdepth == 24 && (bufsz == 24 || bufsz - alpha == 24) && btr > 0) usable24 = 1;
        if (vdepth == 32 && (bufsz == 32 || bufsz - alpha == 32) && btra > 0) usable32 = 1;
    }
    printf("\nUSABLE TFP config for depth 24: %s\n", usable24 ? "YES" : "NO");
    printf("USABLE TFP config for depth 32: %s\n", usable32 ? "YES" : "NO");
    printf("\n(If depth-24 = NO, that's exactly why muffin falls back.\n"
           " Compare each printed attr to what yserver sent: visualDepth must be\n"
           " 24/32 via glXGetVisualFromFBConfig, and BIND_RGB/BIND_RGBA must be 1.\n"
           " A -999 means mesa returned BadAttribute — it never stored that attr.)\n");
    return 0;
}
