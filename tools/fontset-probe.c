/* XCreateFontSet probe — e16's alert init creates a fontset for
 * "fixed"; verify whether it succeeds against the target server.
 * rc=0 fontset created, rc=1 NULL fontset. */
#include <X11/Xlib.h>
#include <stdio.h>
#include <locale.h>
int main(void) {
    setlocale(LC_ALL, "");
    Display *d = XOpenDisplay(NULL);
    if (!d) { printf("no display\n"); return 2; }
    char **missing = NULL; int nmissing = 0; char *def = NULL;
    XFontSet fs = XCreateFontSet(d, "fixed", &missing, &nmissing, &def);
    printf("fontset=%s nmissing=%d def=%s\n", fs ? "OK" : "NULL", nmissing, def ? def : "(nil)");
    for (int i = 0; i < nmissing; i++) printf("missing: %s\n", missing[i]);
    return fs ? 0 : 1;
}
