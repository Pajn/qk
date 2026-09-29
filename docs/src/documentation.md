# Maintaining the documentation

The book's Markdown sources live in `docs/src`. `docs/src/SUMMARY.md`
defines the sidebar, and `docs/book.toml` configures the theme and built-in
search. Add new pages to the summary so they are built and indexed.

## Build and preview

Install the same mdBook version as CI, then run these commands from the
repository root:

```sh
cargo install mdbook --version 0.5.4 --locked
mdbook build docs
mdbook serve docs --open
```

`mdbook serve` watches the source files and rebuilds as you edit. Generated
HTML goes into `docs/book/`, which Git ignores. Search runs in the browser
using the index generated during the build; no external search service is
needed. Press `/` or `s` to open search.

## Publish on GitHub Pages

Once the repository is hosted on GitHub:

1. In **Settings → Pages → Build and deployment**, set **Source** to
   **GitHub Actions**.
2. Push the documentation workflow to the repository's default branch.
3. Open the **Documentation** workflow run. Its deployment links to the site.

The workflow builds documentation on pull requests and pushes that touch its
sources. Only the default branch deploys; **Run workflow** can also deploy
from that branch. The `github-pages` environment records deployments.
The build and deployment jobs use GitHub's
[Pages artifact workflow](https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages).
Generated files are uploaded as an artifact rather than committed to a branch.

The workflow obtains the site's base path from GitHub Pages, so it also
works with a renamed repository or a custom domain. `site-url` in
`docs/book.toml` is the fallback for local builds. CI also supplies a link to
the GitHub repository without hardcoding an owner in the source.

## Writing reference pages

Document implemented behavior, with types, defaults, precedence and a small
example. Give configuration keys their own headings so search results can
link straight to the option. Explain when an accepted Nx setting has no
effect in qk. Keep examples aligned with the fixtures and implementation.
Use relative Markdown links between pages, and label design proposals clearly.

Before committing, run `mdbook build docs` and check changed pages and search
in the preview. Pull requests build the book but do not publish it.
