#!/usr/bin/env python3
"""Cut the Noto Sans CJK faces down to the characters the translations use (a few hundred glyphs
instead of 65 000: kilobytes instead of 16 MB each). Run it again after changing zh, ja or ko:

    python3 crates/gg-desk/assets/fonts/subset_cjk.py [/path/to/NotoSansCJK-Regular.ttc]

It needs fontTools (`pip install fonttools`). The test `every_character_of_every_language_has_a_glyph`
fails when a translation uses a character the subsets lack.
"""
import pathlib
import sys
import tomllib

from fontTools import subset
from fontTools.ttLib import TTFont

HERE = pathlib.Path(__file__).resolve().parent
I18N = HERE.parent.parent / "i18n"
SOURCE = sys.argv[1] if len(sys.argv) > 1 else "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"
# Face of the collection for each language, and the language names shown in every language.
FACES = {"ja": 0, "ko": 1, "zh": 2}
NAMES = "中文日本語한국어"
ASCII = "".join(chr(c) for c in range(0x20, 0x7F))

for lang, face in FACES.items():
    table = tomllib.loads((I18N / f"{lang}.toml").read_text(encoding="utf-8"))
    text = "".join(table.values()) + NAMES + ASCII + "·…–—°％（）：、。，！？「」"
    options = subset.Options()
    options.layout_features = ["*"]
    options.name_IDs = ["*"]
    options.notdef_outline = True
    font = TTFont(SOURCE, fontNumber=face, lazy=False)
    subsetter = subset.Subsetter(options)
    subsetter.populate(text=text)
    subsetter.subset(font)
    out = HERE / f"NotoSansCJK-{lang}.otf"
    subset.save_font(font, str(out), options)
    print(f"{out.name}: {len(set(text))} characters, {out.stat().st_size} bytes")
