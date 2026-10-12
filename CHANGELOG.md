# Changelog

Each `## <version>` section becomes the notes of that version's GitHub
Release (CI's `release` job).

## 0.12.0

- **Download models in the app.** Settings → Models has a "Recommended
  models" card. One click downloads a builder and a checker through Ollama,
  with a progress bar. Without Ollama, the card links to its download page.
- **Easy updates.** Installers are published on the Releases page. The app
  shows a banner when a newer version is out (Settings → General → Check
  for updates; it asks GitHub, and sends nothing about you).
- **The checker runs your tests.** Set a test command per project, or
  accept the one Anchor suggests. After each change the tests run on a copy
  of the project, without network. A change that makes them fail goes back
  to the builder with the output.
- **Reply to a task.** When a task is open, the task box continues it: the
  builder keeps its conversation, and the checker reviews the new change.
  *New task* still starts fresh.

## 0.11.0

- Local models work on Windows (fixes "os error 10106" with Ollama).
- **Two AIs.** Builder + checker (the default): a second model of another
  family reviews each change and sends it back with notes. Two builders:
  both work at once, and you confirm each merge. One AI: as before.
