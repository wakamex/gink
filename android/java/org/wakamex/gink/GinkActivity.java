package org.wakamex.gink;

import org.libsdl.app.SDLActivity;

// gink's window on Android: SDL's activity, running main() from libmain.so, which has SDL linked in.
public class GinkActivity extends SDLActivity {
    @Override
    protected String[] getLibraries() {
        return new String[] { "main" };
    }
}
