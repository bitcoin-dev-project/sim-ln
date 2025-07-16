## Contributing 

SimLN is open and welcomes contributions from the community. A good place to start if you have questions is our [discussions forum](https://github.com/bitcoin-dev-project/sim-ln/discussions/123). 

## Pull Request (PR) Requirements

The list below contains mandatory checks to be adhered to before a PR can be opened and/or reviewed. 

1. [Testing](#testing)
2. [Formating and Linting](#formatting-and-linting)
4. [Commit history](#commit-history)

**Note**: Reviews are contigent on these checks passing locally and in CI.

### Testing

All tests must pass. Do so by running the command

```sh
$ cargo test
```

### Formatting and Linting

Formatting and linting should adhere to the rules specified by default and/or specifically in `rustfmt.toml`. Adhere to the linter suggestions made by `clippy`.
In addition to these check, the recipe `stable-output` ensures consistent and reliable output locally and in CI environment.

```sh
$ make check
```

### Commit history

This project strictly adheres to an [atomic commit structure](https://en.wikipedia.org/wiki/Atomic_commit#Atomic_commit_convention). This means that:
- Each commit should be a small, coherent logical change described with a [good commit message](https://tbaggery.com/2008/04/19/a-note-about-git-commit-messages.html)
- Refactors, formatting changes and typo fixes should be in separate commits to logical changes
- Every commit should compile and pass all tests so that we can use [git bisect](https://git-scm.com/docs/git-bisect) for debugging

When your PR is in review, changes should be made using [fixup commits](https://andrewlock.net/smoother-rebases-with-auto-squashing-git-commits/) to maintain the structure of your commits.
When your PR is complete, reviewers will give the instruction to `squash` the fixup commits before merge.

Some tips for using fixup commits:
- Ordering: put fixup commits directly after the commit they're fixing up. This allows reviewers to use "shift + select" on github to review the full commit, and can easily squash them themselves.
- Use your discretion: if your PR has not had much review yet, or is a single commit then fixups add less value. 
- Communication: one set procedure isn't going to fit every PR - if you feel that squashing your fixups would be helpful, ask your reviewer! If you don't think they're worth using at all, motivate why!

For historical reasons, commit titles in the project are formatted as `<scope>/<type>: commit message`. A list of different commit types is available [here](https://graphite.dev/guides/git-commit-message-best-practices#2-types-of-commits).

For example, a commit that refactors the `sim-cli` package will be titled: `sim-cli/refactor: {message about refactor}`.
