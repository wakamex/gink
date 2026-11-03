# Bundled files

## ClankerTwemoji.ttf

A colour emoji font installed beside the program and used ahead of the system's emoji font on Windows and Linux, so that flags and family sequences draw as one glyph each.

- Graphics: [Twemoji](https://github.com/jdecked/twemoji) 17.0.3, copyright Twitter, Inc. and other contributors, licensed under [Creative Commons Attribution 4.0 International (CC BY 4.0)](https://creativecommons.org/licenses/by/4.0/).
- Changes: the graphics were converted to a font in the [COLR](https://learn.microsoft.com/en-us/typography/opentype/spec/colr) version 0 format with every emoji one em wide; they were not redrawn.
- Source and build script: [wakamex/fonts](https://github.com/wakamex/fonts), `build_twemoji.py`.

## ClankerMono-Regular.ttf

A monospaced text font installed beside the program, which the window can use in place of the system's font.

- Font: Clanker Mono, a renamed build of [D2Coding](https://github.com/naver/d2-coding-font) 1.4.0 by NAVER Corp., licensed under the [SIL Open Font License 1.1](https://openfontlicense.org/), whose text is in `ClankerMono-LICENSE.txt` here. D2Coding's license reserves the name "D2Coding", which is why a modified build carries another name.
- Changes: tighter line spacing and hinting fixes for a few glyphs; the outlines, widths and Hangul are D2Coding's.
- Source and build scripts: [wakamex/fonts](https://github.com/wakamex/fonts).

## Inter-Regular.ttf

A text font installed beside the program, which the window can use in place of the system's font. It is the font Signal's own desktop program draws its text with.

- Font: [Inter](https://rsms.me/inter/) 4.1 by The Inter Project Authors, unmodified, licensed under the SIL Open Font License 1.1, whose text is in `Inter-LICENSE.txt` here.
