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

- Have a clean commit history: It is preferable to use [fixup and squash](https://andrewlock.net/smoother-rebases-with-auto-squashing-git-commits/) in addressing feedback from PR review. The former (i.e. `fixup`) for the commits they apply to (during review), and latter (`squash`) once review is complete.
- Use [good commit messages](https://tbaggery.com/2008/04/19/a-note-about-git-commit-messages.html)
- Resolve all conflicts
- Rebase often
