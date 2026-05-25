# Third-party notices

**zapret-ui** is an independent graphical front-end. It is licensed under the
[MIT License](LICENSE) and does **not** bundle or redistribute the DPI-bypass
engine — it downloads the upstream distribution at runtime, on the user's
machine, directly from the original authors' repositories.

The downloaded components remain the property of their respective authors and
are governed by their own licenses:

| Component | Author | Role | License |
|-----------|--------|------|---------|
| [zapret-discord-youtube](https://github.com/Flowseal/zapret-discord-youtube) | Flowseal | Ready-made strategy presets + binaries that zapret-ui downloads and runs | See upstream repository |
| [zapret](https://github.com/bol-van/zapret) | bol-van | The underlying DPI-bypass engine (`winws`) | MIT |
| [WinDivert](https://github.com/basil00/WinDivert) | basil00 | Windows packet-capture driver used by `winws` | LGPLv3 / GPLv2 |

Because zapret-ui ships none of the above in its own binary, only this MIT
license applies to the contents of this repository. When the application
downloads the upstream distribution, the licenses of those components apply to
the downloaded files.

This project is **not affiliated with or endorsed by** Flowseal, bol-van, or
the WinDivert authors.
