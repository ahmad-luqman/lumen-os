# The desktop's wallpaper

`lumen.png`: 1280x800 RGB, about 150 KB. It is an original abstract image, a diagonal gradient with four soft discs of light. `tools/wallpaper.py` generates it, and it is under Lumen's MIT licence (`LICENSE`), like the rest of the repository.

The kernel embeds the file (`kernel/src/desktop.rs`) and decodes it once a boot, when the desktop first starts. See ARCHITECTURE section 11, "Text and images".

sha256: `d338c4a8a8dbf60ddfed8ea4f12ece82228937c5de6b68e7279c8d468a7dce8e`

Running `uv run python tools/wallpaper.py` with the Pillow locked in `uv.lock` (13.0.0) rewrites the file byte for byte. To change the wallpaper:
1. Edit the script and run it again.
2. Look at the image.
3. Re-record the desktop goldens with `make golden`, and look at those too.
