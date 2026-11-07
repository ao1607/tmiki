"""Format readers used by tmi importers.

The modules that open images through Pillow install tmi's image-size limit
first (``pixel_limit.apply_to_pil``, see clip.py); the MDP2 reader does not
use Pillow (RUST_MIGRATION.md 2c)."""
