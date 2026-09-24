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

When the application downloads the upstream distribution, the licenses of those
components apply to the downloaded files. Rust dependencies retain their own
licenses.

This project is **not affiliated with or endorsed by** Flowseal, bol-van, or
the WinDivert authors.

## Telegram proxy

The native Rust MTProto-to-WebSocket module was written using the protocol and
routing approach demonstrated by [Flowseal/tg-ws-proxy](https://github.com/Flowseal/tg-ws-proxy).
It does not bundle or execute that project's Python implementation. The following
notice acknowledges the reference implementation:

MIT License

Copyright (c) 2026 Flowseal

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
