# openboard.lol

The public marketing website. Buildless HTML, CSS and JavaScript; no package install required.

## Preview and check

From the repository root:

```sh
python3 website/check.py
python3 -m http.server 8765 --directory website
```

Open http://localhost:8765. The copy button requires a secure context (localhost or HTTPS).
Typography uses Google Fonts with local fallbacks; no analytics or tracking scripts are included.

## Source and publishing

Keep `website/` **on main**, beside the product. Agents and contributors see the site in their normal checkout, can keep claims aligned with shipped functionality, and can review product and marketing changes together. A dedicated source-only `gh-pages` branch would hide this context and encourage drift. The implementation branch `website/github-pages` is a review branch intended to merge into main, not a long-lived second source of truth.

The Pages workflow validates pull requests and deploys only from main. It uploads an allowlist of public assets; internal documentation and checks are excluded. No generated branch is needed.

One-time repository setup after merging:

1. In repository Settings → Pages, select **GitHub Actions** as the build source.
2. Set the custom domain to **openboard.lol** and configure the domain’s DNS for GitHub Pages. Follow GitHub’s current Pages custom-domain instructions, including domain verification.
3. Enable HTTPS once the certificate is ready.
4. Run the Public website workflow, or change a file in `website/` on main.

`CNAME` records the intended domain for contributors, but GitHub ignores it for Actions deployments. Configure the custom domain in repository Pages settings; DNS is managed separately. No deployment or DNS changes have been made by this branch.

## Editing

- `index.html`: copy, links, status, install command, illustrative handoff.
- `style.css`: responsive layout and pink/yellow/black design tokens.
- `site.js`: copy command with accessible success/failure feedback.
- `icon.svg`: site icon.

Keep the install line synchronized with the root README. Tasks, grants and the TypeScript SDK (`openboardlol` on npm) have shipped. Do not promote signed approvals, a hosted board, or bridges as available until the product ships them. The handoff is an illustration, not a live product screenshot or a bundled ChatGPT/Claude integration. Tools need CLI access; Openboard does not grant computer-use abilities.
