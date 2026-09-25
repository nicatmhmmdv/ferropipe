# Bundled fonts (optional)

Ferropipe discovers its fonts at runtime and needs nothing in this directory.
It looks for IBM Plex first, then Noto Sans / Cantarell / JetBrains Mono, then
DejaVu, searching `assets/fonts` next to the executable and in the working
directory, `~/.local/share/fonts`, `~/.fonts`, `/usr/local/share/fonts` and
`/usr/share/fonts`.

To embed IBM Plex into the binary instead, place these four files here and build
with `cargo build --features bundled-fonts`:

- `IBMPlexSans-Text.ttf`
- `IBMPlexSans-Medium.ttf`
- `IBMPlexMono-Regular.ttf`
- `IBMPlexSansCondensed-SemiBold.ttf`

They come from the IBM Plex release (https://github.com/IBM/plex) and are
licensed under the SIL Open Font License 1.1; keep the OFL text alongside them
when redistributing.

Bundling covers only these four UI faces. The wide-coverage symbol fallbacks
(DejaVu Sans, Noto Sans Symbols 2) are still picked up from the system font
directories when present.

On Arch Linux, `pacman -S ttf-ibm-plex` installs the same faces system-wide and
runtime discovery picks them up with no bundling at all.
