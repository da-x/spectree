# spectree

A tool for building dependent [RPM](https://en.wikipedia.org/wiki/RPM_Package_Manager) packages from a YAML specification with support for multiple build backends and parallel execution.

I used this to solve the following problem: given a tree of source RPMs, which depend on one another, I want to build them in dependency order and in parallel where possible.


## Features

- **Dependency Management**: Automatically resolves and builds packages in correct dependency order
- **Parallel Builds**: Concurrent execution of independent builds
- **Multiple Backends**: Support for Mock, Docker, and Copr backends
- **Smart Caching**: Avoids rebuilding packages that haven't changed
- **Remote Builds**: Full support for Fedora Copr remote builds with state tracking
- **Debug Mode**: Special debug mode for patching up packages (stop after `rpmbuild -bp`).


## Installation


```bash
git clone https://github.com/da-x/spectree
cd spectree
cargo install --path .
```


## Real world usage example

I have the following Git clones of source RPMs:

```
drwxrwxr-x 4 dan dan 4096 Sep  4 17:37 bitstream-vera-fonts
drwxr-xr-x 4 dan dan 4096 Oct  1 08:10 clementine
drwxr-xr-x 4 dan dan 4096 Sep  4 17:54 clementine.repo
drwxr-xr-x 4 dan dan 4096 Sep  4 18:28 cryptopp
drwxr-xr-x 4 dan dan 4096 Sep  4 18:10 glm
drwxr-xr-x 4 dan dan 4096 Sep  4 17:43 jack-audio-connection-kit
drwxrwxr-x 4 dan dan 4096 Sep  4 17:41 libmygpo-qt
drwxrwxr-x 4 dan dan 4096 Sep  4 17:20 libprojectM
drwxrwxr-x 4 dan dan 4096 Sep  4 17:07 libqxt-qt5
drwxrwxr-x 4 dan dan 4096 Sep  4 17:06 qtiocompressor
drwxr-xr-x 4 dan dan 4096 Oct  1 10:20 qtlockedfile
drwxr-xr-x 4 dan dan 4096 Oct  1 10:22 qtsingleapplication
drwxrwxr-x 4 dan dan 4096 Sep 30 20:52 sha2
drwxr-xr-x 4 dan dan 4096 Sep  4 18:24 sparsehash
```
```
```

I used the following configuration in order to build Clementine for Rocky Linux 10:

```
sha2: {type: {source: git, path: "${NAME}" }}
qtiocompressor: {type: {source: git, path: "${NAME}" }}
libqxt-qt5: {type: {source: git, path: "${NAME}" }}
bitstream-vera-fonts: {type: {source: git, path: "${NAME}" }}
libmygpo-qt: {type: {source: git, path: "${NAME}" }}
jack-audio-connection-kit: {type: {source: git, path: "${NAME}" }}
glm: {type: {source: git, path: "${NAME}" }}
sparsehash: {type: {source: git, path: "${NAME}" }}
cryptopp: {type: {source: git, path: "${NAME}" }}
libprojectM: {type: {source: git, path: "${NAME}" }, dependencies: ["bitstream-vera-fonts", "glm", "jack-audio-connection-kit"]}
qtlockedfile: {type: {source: git, path: "${NAME}" }}
qtsingleapplication: {type: {source: git, path: "${NAME}"}, dependencies: ["qtlockedfile"]}
clementine: {type: {source: git, path: "${NAME}"}, dependencies: ["sha2", "qtsingleapplication", "libprojectM", "qtiocompressor", "libqxt-qt5", "sparsehash", "libmygpo-qt", "libqxt-qt5"]}
```


(the builds are posted [here](https://copr.fedorainfracloud.org/coprs/alonid/clementine/builds/).

## Quick Start

Create a YAML specification file (`packages.yaml`):

```yaml
library:
  source: git
  path: /path/to/local/git-clone/database
  dependencies: []

remote-library:
  source: git
  url: https://src.fedoraproject.org/rpms/bash

app:
  source: git
  path: /path/to/local/git-clone/app
  dependencies:
    - remote-library
    - library
```

Build with Mock (default):

```bash
spectree packages.yaml /workspace app
```

## Build Backends

### Mock Backend (Default)

Uses Mock to build packages in isolated chroots:

```bash
spectree packages.yaml /workspace app --backend mock
```

### Docker Backend

Builds packages in Docker containers with automatic dependency resolution:

```bash
spectree packages.yaml /workspace app --backend docker --target-os fedora-39
```

**Debug Mode**: For investigating build issues, use the debug flag to stop after source preparation:

```bash
spectree packages.yaml /workspace app --backend docker --debug-prepare
```


This runs `rpmbuild -bp` (prepare only), prints the prepared source path, and intentionally fails so you can inspect the BUILD directory.

### Copr Backend
Submits builds to Fedora Copr for remote building:

```bash
spectree packages.yaml /workspace app \
  --backend copr \
  --copr-project myproject \
  --copr-state-file builds.yaml \
  --exclude-chroot fedora-38-x86_64 \
  --copr-assume-built "^(glibc|gcc|binutils).*"
```

Copr builds include:
- **State Tracking**: Persistent state file tracks build status across runs
- **Smart Resumption**: Automatically resumes interrupted builds
- **Chroot Exclusion**: Skip specific architectures/distributions
- **Build Status Management**: Handles submitted, in-progress, completed, and failed states
- **Assume Built**: Skip packages matching regex pattern (useful for packages already available in Copr build)


### Null Backend

Useful for testing dependency resolution without actual builds:
```bash
spectree packages.yaml /workspace app --backend null
```


## YAML Specification

### Source Types

#### Git Sources

```yaml
package-name:
  source: git
  url: https://github.com/user/repo.git
  dependencies:
    - dependency1
    - dependency2
  build_params:
    - "--define"
    - "custom_param value"
```


#### Git Sources with Local Paths

```yaml
package-name:
  source: git
  path: /local/path/to/repo
  dependencies: []
```


#### Git Sources with Path Templates

```yaml
package-name:
  source: git
  path: /repos/${NAME}  # ${NAME} gets replaced with package name
  dependencies: []
```

#### SRPM Sources (Planned)

```yaml
package-name:
  source: srpm
  path: /path/to/package.src.rpm
  dependencies: []
```

### Dependency Types

#### Regular Dependencies
```yaml
dependencies:
  - package1  # Regular dependency - includes transitive dependencies
  - package2
```

#### Direct-Only Dependencies
```yaml
dependencies:
  - ~package1  # Direct-only dependency - RPMs are not added to repos during dependent of dependent build.
  - package2   # Regular dependency
```


Direct-only dependencies (prefixed with `~`) are useful when bootstrapping packages, e.g. `gcc-bootstrap` -> `binutils` -> `gcc`.

**TO DO**: support package build parameters


## Command Line Options

```
spectree [OPTIONS] <SPEC_FILE> <WORKSPACE> <ROOT_SOURCE>

Arguments:
  <SPEC_FILE>    Path to the YAML specification file
  <WORKSPACE>    Workspace directory for builds and Git clones
  <ROOT_SOURCE>  Root source to start building from

Options:
  -b, --backend <BACKEND>
          Builder backend [default: mock] [possible values: mock, null, docker, copr]

      --target-os <TARGET_OS>
          Target OS for Docker backend (e.g., fedora-39, centos-stream-9)

      --copr-project <Copr_PROJECT>
          Copr project name (required for Copr backend)

      --copr-state-file <Copr_STATE_FILE>
          YAML file to store Copr build state mappings (required for Copr backend)

      --exclude-chroot <EXCLUDE_CHROOT>
          Exclude chroot for Copr builds (can be specified multiple times)

      --copr-assume-built <Copr_ASSUME_BUILT>
          Regex pattern for source keys to assume are already built in Copr (skip building)

      --debug-prepare
          Debug mode: only prepare sources (rpmbuild -bp) and leave them for inspection

  -h, --help
          Print help
```


## Build Artifacts

The workspace argument provides a directory in which the tool maintains its temporary state and its final outputs.

### Local Builds (Mock/Docker)

```
workspace/
├── sources/          # Git repository clones (only for remotes!)
│   ├── package1/
│   └── package2/
└── builds/           # Build artifacts
    ├── package1-abc123/
    │   ├── deps/     # Hardlinked deps repo
    │   └── build/    # RPM files
    └── package2-def456/
        └── build/    # RPM files
```


### Remote Builds (Copr)

Build artifacts remain on Copr infrastructure. Local workspace only contains source preparation, in that case `builds/` is not maintained and we only need to clone source RPMs from Git if we are working with remotes.


## Advanced Features

### Build Hashing
SpecTree uses SHA256 hashing to determine when packages need rebuilding:
- Source content hash (Git tree hash)
- Dependency build hashes
- Build parameters
- Spec file changes

Only packages with changed hashes are rebuilt, making incremental builds very fast.


## Requirements

- Rust 1.70+
- Git
- For Mock backend: Mock, createrepo_c
- For Docker backend: Docker, createrepo_c
- For Copr backend: copr-cli


## Limitations and To Dos

There are a lot of ways in which this can be extended.

Here's some of the stuff on the To do:

- [ ] Allow to limit parallelism
- [ ] Support more target RPM distributions and versions
- [ ] Support Debian/Ubuntu packages?
- [ ] Print the build tree (e.g. dry run)
- [ ] Docker build: save the build log along with the output like 'mock' does
- [ ] For Copr builds, support built-pruning direct-only dependencies
- [ ] For non-remote build, auto-delete failed builds, and add '--keep-failed' argument to disable that.
- [ ] Allow to prune workspace builds that are not currently in the root set
- [ ] Docker build emits no output while it is running
- [ ] Make it clearer in the info prints about missing RPM dependencies

## License

This project is licensed under the BSD 2 Clause License - see the LICENSE file for details.
