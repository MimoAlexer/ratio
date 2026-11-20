# GitHub delivery

The source, CI and desktop installers are in the [private Working Set repository](https://github.com/MimoAlexer/working-set). The [release](https://github.com/MimoAlexer/working-set/releases/tag/v0.1.0) contains the two installers and their checksums. The [private Delivery board](https://github.com/users/MimoAlexer/projects/4/views/2) is linked to the repository and tracks all ten release and roadmap issues. The [issue list](https://github.com/MimoAlexer/working-set/issues) and [milestones](https://github.com/MimoAlexer/working-set/milestones) track delivery and hosted-service qualification.

GitHub authorizes a personal Projects board separately from access to a private repository. From a GitHub CLI session authenticated as the owner with the `project` scope, the following idempotent command creates or reconciles the private board, links it to the repository, adds the ten tracked issues, and verifies visibility and issue status. It does not change the repository visibility.

```bash
python3 ops/github-board.py MimoAlexer/working-set 1 2 3 4 5 6 7 8 9 10
```
