# GitHub delivery

The initial launch targets the website. Windows and Linux distribution is [planned after the web launch](https://github.com/MimoAlexer/ratio/issues/12). The [private Delivery board](https://github.com/users/MimoAlexer/projects/4/views/2) tracks the roadmap. Earlier desktop builds remain in the private release history.

GitHub authorizes a personal Projects board separately from access to a private repository. From a GitHub CLI session authenticated as the owner with the `project` scope, the following idempotent command reconciles the existing linked private board (#4), adds any missing tracked issues, and verifies visibility and issue status. It does not change the repository visibility.

```bash
python3 ops/github-board.py MimoAlexer/ratio 1 2 3 4 5 6 7 8 9 10
```
