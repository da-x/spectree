Under Fedora/CentOS/RHEL, this program takes a YAML definition as an input file, and invokes `mock` in order to build RPM packages that are dependent on another.

The YAML file describes a tree.
Each source is identified by a unique string key (e.g. `binutils`), and has the fields:
- How to obtain the source RPM (enum field)
    - From Some Git URL (can be local file://<path>)
    - Prebuilt source RPM
- Dependencies: List of string of other source keys.
- If there are non-default build params, we specify them. These will be passed to `mock` directly (e.g., `-D`, or `--with`, `--without`).

Operation:
- Build key: resolve each source by combining the source key with the hash of its content, meaning that if it's a Git tree we will take the Git tree hash. Refuse to work with a Git tree that has an unclean Git status. We will also include the fields into the hash. Use SHA256. Build key is of form `<source-key>-<hash>`.
- There is a workspace directory where each build will have a `<build-key>` subdir.
- Prepare dependencies as `<build-key>/deps` by hardlinking all the builds we indirectly depend on into `<build-key>/deps/<dep-buiid-key>`, for each build's `<build-key>/build` directory. We will be running `createrepo_c` over `<build-key>/deps` so that `<build-key>/deps/repodata` is created and we can use `<build-key>/deps` as a repo directory when executing `mock` (use `--addrepo`). If a key starts with `~` it is only take if it is a direct dependency, and we strip the `~`.
- Invoke `mock` so that it builds `<build-key>/build`. If `<build-key>` already exists we do nothing.
