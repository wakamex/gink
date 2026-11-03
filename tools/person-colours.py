"""Makes the list of colours that people's names are drawn in (person_colours in src/main.c).

    uv run --with numpy tools/person-colours.py [COUNT]

The procedure is the one behind wakterm's tab colours: of every sRGB colour (here every fourth value
of each channel) keep those of a lightness and a colourfulness that read as text on the window's dark
grey, measured in Oklab, where equal distances look about equally different; then pick them one at a
time, each the colour farthest from all those picked before it. The account's own green and the
text's near-white count as picked from the start, so no person is given a colour close to either.
Any first part of the list is therefore as varied as that many colours can be.
"""

import sys

import numpy as np

LIGHTNESS = (0.74, 0.86)
CHROMA = (0.09, 0.18)
TAKEN = [(0.208, 0.761, 0.647), (0.86, 0.86, 0.86)]  # the accent, and the text


def oklab(rgb):
    linear = np.where(rgb <= 0.04045, rgb / 12.92, ((rgb + 0.055) / 1.055) ** 2.4)
    lms = linear @ np.array([[0.4122214708, 0.2119034982, 0.0883024619], [0.5363325363, 0.6806995451, 0.2817188376], [0.0514459929, 0.1073969566, 0.6299787005]])
    return np.cbrt(lms) @ np.array([[0.2104542553, 1.9779984951, 0.0259040371], [0.7936177850, -2.4285922050, 0.7827717662], [-0.0040720468, 0.4505937099, -0.8086757660]])


def main():
    count = int(sys.argv[1]) if len(sys.argv) > 1 else 64
    values = np.arange(0, 256, 4)
    rgb = np.stack(np.meshgrid(values, values, values, indexing="ij"), -1).reshape(-1, 3)
    lab = oklab(rgb / 255.0)
    chroma = np.hypot(lab[:, 1], lab[:, 2])
    keep = (lab[:, 0] >= LIGHTNESS[0]) & (lab[:, 0] <= LIGHTNESS[1]) & (chroma >= CHROMA[0]) & (chroma <= CHROMA[1])
    rgb, lab = rgb[keep], lab[keep]
    nearest = np.full(len(lab), np.inf)
    for taken in oklab(np.array(TAKEN)):
        nearest = np.minimum(nearest, np.linalg.norm(lab - taken, axis=1))
    picked = []
    for _ in range(count):
        best = int(np.argmax(nearest))
        picked.append(rgb[best])
        nearest = np.minimum(nearest, np.linalg.norm(lab - lab[best], axis=1))
    for start in range(0, count, 8):
        print("    " + ", ".join(f"0x{r:02x}{g:02x}{b:02x}" for r, g, b in picked[start : start + 8]) + ",")


if __name__ == "__main__":
    main()
