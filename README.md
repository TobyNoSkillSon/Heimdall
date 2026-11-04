<p align="center">
  <img src="docs/images/icon.png" alt="" width="88">
</p>

<h1 align="center">Heimdall <img src="docs/images/alpha.svg" alt="Alpha" width="58" height="24"></h1>

<p align="center">An assistant you talk to that sees and hears your Mac, instead of a chat window.</p>

<p align="center">
  <a href="https://github.com/TobyNoSkillSon/Heimdall/releases"><img src="docs/images/download.svg" alt="Download" width="152" height="42"></a>
  &nbsp;
  <a href="https://tobynoskillson.com/heimdall"><img src="docs/images/website.svg" alt="Website" width="140" height="42"></a>
  &nbsp;
  <a href="https://tobynoskillson.com/#support"><img src="docs/images/support.svg" alt="Support the Developer on GitHub Sponsors or Buy Me a Coffee" width="228" height="42"></a>
</p>

<p>
<picture>
  <source media="(max-width: 600px)" srcset="docs/images/readme-hero-phone.webp">
  <img src="docs/images/readme-hero.webp" alt="A Mac desktop with Heimdall: the notch says the build is green, Goals and Scheduled cards on the left, a fine-tune job and a trip question on the right">
</picture>
</p>

Say it out loud. Heimdall hears you without a wake word, hands the job to an agent, and shows it as a small card on your
screen. It asks only when it must.

<p align="center">
  <img src="docs/images/island-site.webp" alt="A landing page job card: the page's picture, its score and two judges' lines" width="359">
  <img src="docs/images/island-film.webp" alt="A film restore job card: an 1897 frame, 212 of 480 frames done" width="359">
</p>

<p>
  <img src="docs/images/readme-islands.webp" alt="The Today timeline and the Settled card: what got done this week">
</p>

Listening, transcription, speaker naming, the wake decision, memory search and the voice run on your Mac. Only the
agent's reasoning goes to the provider you connect.

## Alpha

The source code and the first alpha release, v0.1.0-alpha, are coming shortly.

- A Mac with Apple silicon and macOS 26 or newer; 32 GB of RAM recommended.
- About 6 GB of disk for the on-device models.
- One agent provider: Claude through T3 Code, a ChatGPT or OpenCode Go subscription, or an Anthropic or OpenRouter
  API key.

The wake decision model is [heimdall-d1-v6](https://huggingface.co/Toby-NoSkillSon/heimdall-d1-v6) on Hugging Face.
For more on-device models on your Mac, see [Arsenal](https://github.com/TobyNoSkillSon/Arsenal).

## License

Apache-2.0, see [LICENSE](LICENSE).
