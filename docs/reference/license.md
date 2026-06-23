# License

rstest is distributed under the **MIT license**, maintained by Kovant AB.

## Vendored software

rstest ships a vendored copy of [pytest](https://pytest.org) (currently
**9.0.3**) inside the `rstest_worker._vendor` package. pytest is MIT
licensed:

> Copyright (c) 2004 Holger Krekel and others
>
> Permission is hereby granted, free of charge, to any person obtaining a
> copy of this software and associated documentation files (the
> "Software"), to deal in the Software without restriction, including
> without limitation the rights to use, copy, modify, merge, publish,
> distribute, sublicense, and/or sell copies of the Software, and to
> permit persons to whom the Software is furnished to do so, subject to
> the following conditions:
>
> The above copyright notice and this permission notice shall be included
> in all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
> OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
> MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
> IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
> CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
> TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
> SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

The vendored copy is unmodified (`python/VENDOR.md` in the repository
documents provenance and the update procedure). rstest's runtime depends
on [pluggy](https://github.com/pytest-dev/pluggy) (MIT) — pytest's own
plugin framework — as a regular dependency, so plugins keep class identity
with the real library.
