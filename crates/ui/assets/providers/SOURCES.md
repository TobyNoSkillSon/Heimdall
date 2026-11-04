# Provider icons

Menu-row images for Inference Engine ▸ API Providers: 16×16 pt, `name.png` (16 px) and `name@2x.png` (32 px), black on transparent.

| File | Mark | Source (fetched 2026-10-18) |
|---|---|---|
| `anthropic.png`, `anthropic@2x.png` | Anthropic mark (A\\) | https://www.anthropic.com — the inline SVG of the site's mark, `viewBox="0 0 46 32"`: the stroke `M32.73 0H25.7846L38.4499 32H45.3953L32.73 0Z` and the A `M12.6653 0L0 32H7.08167L9.67193 25.28H22.9219L25.5122 32H32.5939L19.9286 0H12.6653ZM11.9626 19.3371L16.2969 8.09143L20.6313 19.3371H11.9626Z` (both `currentColor`). |
| `opencode.png`, `opencode@2x.png` | OpenCode mark | https://opencode.ai/favicon.svg — the frame path `M384 416H128V96H384V416ZM320 160H192V352H320V160Z` (white) and the block `M320 224V352H192V224H320Z` (`#5A5858`), without the favicon's `#131010` square. The same geometry is the mark in OpenCode's brand assets (`packages/console/app/src/asset/brand/opencode-logo-*-square.svg` in github.com/anomalyco/opencode). |

The shapes are the source vectors, unedited: the mark was cropped to its own bounds (256 × 320 units), rasterised by exact pixel coverage and centred on the 16-pt canvas. It is 12.5 pt tall, so at 2× each 64-unit stroke is exactly 5 px and every edge lands on a pixel; a taller mark would outweigh the outline SF Symbols beside it. The block keeps the favicon's two tones as 30 % opacity, the grey's position between the favicon's ground and its white frame ((90 − 19) / (255 − 19)).

Template, not colour: the mark sits in a column of monochrome SF Symbols, and as a template it turns white on the highlighted row and follows light and dark mode.

Loading: build one `NSImage` of size 16×16 containing both bitmaps (or load the @2x file and `setSize(16, 16)`), then call `setTemplate(true)`. `NSImage(contentsOfFile:)` on the 1x path does not pick up the @2x file by itself.

Anthropic: the same treatment, the paths unedited and filled even-odd (the A's counter stays open). The mark is wide (45.4 × 32 units), so it is sized by width: 14 pt wide, 9.9 pt tall, centred, its baseline on a pixel edge; at that size its strokes match the OpenCode mark's weight. Rasterised by 24 × 24 supersampling per pixel.

| `openai.png`, `openai@2x.png` | OpenAI / ChatGPT mark (the blossom) | ChatGPT for macOS 26.917.71314 (`/Applications/ChatGPT.app/Contents/Resources/chatgptTemplate@2x.png`, the app's own 18 × 18 pt menu-bar template), copied 2026-10-22. |

ChatGPT: the app's own template bitmap, not redrawn: cropped to the mark's bounds (34 × 34 px of the 36 px canvas), resampled (Lanczos) to 14 pt (28 px at 2×, 14 px at 1×) to match the other marks' weight and centred on the 16-pt canvas, black on transparent, alpha kept. Used for the ChatGPT provider (Add Provider ▸ Subscription, Providers, its models and the sign-in panel).
