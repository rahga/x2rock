# Third-Party Notices

x2rock-deck bundles or incorporates the following third-party components:

## Roboto Fonts

- Source: [Google Fonts / Roboto](https://github.com/googlefonts/roboto)
- Files: `ui/assets/fonts/Roboto-Regular.ttf`, `ui/assets/fonts/Roboto-Medium.ttf`, `ui/assets/fonts/Roboto-Bold.ttf`
- License: Apache License, Version 2.0 (see `LICENSE-Roboto.txt`)
- Copyright: Copyright 2011 Google Inc. All Rights Reserved.

## Material Symbols / Icons

- Source: [Google Material Symbols](https://github.com/google/material-design-icons)
- Files: `ui/assets/icons/*.svg`
- License: Apache License, Version 2.0 (see `LICENSE-Roboto.txt`)
- Copyright: Copyright Google LLC

## Slint GUI Toolkit

- Source: [Slint](https://github.com/slint-ui/slint)
- License: GNU General Public License, Version 3 (`GPL-3.0-only`) (see `LICENSE`)
- Copyright: Copyright © SixtyFPS GmbH <info@slint.dev>

## Skia Graphics Library

- Source: [Skia](https://skia.org/), as the prebuilt binaries of
  [rust-skia](https://github.com/rust-skia/rust-skia) (`skia-safe`, `skia-bindings`)
- License: BSD 3-Clause License
- Copyright: Copyright (c) 2011 Google Inc. All rights reserved.
- Statically linked into the executable, with the libraries Skia builds in (below).

```
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

    * Redistributions of source code must retain the above copyright
      notice, this list of conditions and the following disclaimer.
    * Redistributions in binary form must reproduce the above copyright
      notice, this list of conditions and the following disclaimer in the
      documentation and/or other materials provided with the distribution.
    * Neither the name of Google Inc. nor the names of its contributors may
      be used to endorse or promote products derived from this software
      without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE
LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
POSSIBILITY OF SUCH DAMAGE.
```

### Built into Skia

The prebuilt Skia library carries these, found in the library itself; their licence texts are
in Skia's `third_party/` tree.

- **libpng** 1.6: PNG Reference Library License version 2. Copyright (c) 1995-2024 The PNG
  Reference Library Authors.
- **libjpeg-turbo**: IJG License, Modified (3-clause) BSD License and zlib License. Copyright
  (C) 2009-2024 D. R. Commander; based on the Independent JPEG Group's code, Copyright (C)
  1991-2020 Thomas G. Lane, Guido Vollbeding.
- **zlib** 1.3: zlib License. Copyright (C) 1995-2024 Jean-loup Gailly and Mark Adler.
- **Expat** 2.7: MIT License. Copyright (c) 1998-2000 Thai Open Source Software Center Ltd and
  Clark Cooper; Copyright (c) 2001-2025 Expat maintainers.
- **Wuffs**: Apache License, Version 2.0 (see `LICENSE-Roboto.txt`). Copyright 2017 The Wuffs
  Authors.

FreeType and fontconfig are the system's own on Linux, linked dynamically, not bundled.
