# What PyPilot reports

Every finding PyPilot can raise, as `doctor` prints it and as the editor shows it. Titles with `…` vary with the project.

## Environment and Python version

| Finding | Severity | Fix |
|---|---|---|
| No environment set up yet | Warning | **Fix environment** / `pypilot setup` on the recommended Python |
| Python X.Y is not supported by this project | Error | **Fix environment** / `pypilot fix python` |
| Dependencies conflict on Python version | Error | Manual: no single Python works. `pypilot explain` shows what blocks each version |
| Could not resolve `name` | Warning | Manual: a typo or a private package. PyPI answered that it has no such project, or no release matches the pin |
| Could not reach PyPI for some dependencies | Info | <Badge type="tip" text="1.3" /> Re-run online. The lookup never got an answer (offline, a proxy, a PyPI outage), so those packages are listed as *not checked* and constrain nothing; the supported range can look wider than it is |
| Will compile from source | Warning | Manual: no wheel here, the sdist will be built |
| … cannot be installed on *platform* | Error | Manual: the release has no files for this platform |
| uv could not resolve dependencies for Python X.Y | Warning | Manual: `uv --dry-run` found a conflict metadata cannot see (a yanked release, a transitive pin) |
| Conda environment detected | Info | `pypilot migrate-conda` |

A correct environment produces no finding: doctor prints `✓ Everything looks good.` and the editor stays silent.

## GPU and CUDA

| Finding | Severity |
|---|---|
| torch build matched to your driver: cuXXX | Info |
| NVIDIA driver too old for any known CUDA build | Warning |
| NVIDIA driver too old for this torch release / this tensorflow release | Warning |
| torch / tensorflow on Apple Silicon uses MPS | Info |
| tensorflow will run on the CPU on native Windows | Info |
| tensorflow GPU on native Windows needs CUDA 11.2 | Info |
| … needs torch… (a CUDA extension's torch pin does not match) | Warning |
| … would replace torch X (installing the latest extension swaps torch) | Warning |
| … is not in PyPilot's bundled CUDA data | Info |
| … cannot run on macOS | Warning |

See [GPU & CUDA](/guide/gpu-cuda).

## pixi projects

| Finding | Severity |
|---|---|
| Pixi environment not installed yet | Warning |
| The pixi environment has no Python | Warning |
| This pixi project has no pixi.lock yet <Badge type="tip" text="1.3" /> | Info |
| pixi.lock is out of date with *manifest* <Badge type="tip" text="1.3" /> | Warning |
| pixi.lock is out of date and *manifest* no longer solves <Badge type="tip" text="1.3" /> | Error |
| pixi environment `name` does not exist <Badge type="tip" text="1.3" /> | Warning |
| pixi environments run different Python versions <Badge type="tip" text="1.3" /> | Info |
| pixi environment `name` runs Python X.Y, which its PyPI dependencies do not support <Badge type="tip" text="1.3" /> | Error |

See [PyPilot in pixi projects](/pixi/projects).

## The pixi advisory <Badge type="tip" text="1.3" />

| Finding | Severity |
|---|---|
| *package* needs native libraries pip cannot install | Warning |
| CUDA sources found, but nvcc is not installed | Warning |
| This project mixes Python with R / Rust / C/C++ | Info |

Each carries `pypilot migrate-to-pixi` as its fix. See [the advisory](/pixi/advisory).

## Import diagnostics (editor)

Shown on the import line, with `source: "pypilot"`, in open files or <Badge type="tip" text="1.3" /> in every file with [`diagnostics_scope = "workspace"`](./configuration#diagnostics-scope):

| Message | Severity | Quick fix |
|---|---|---|
| `` `pkg` is not installed. `` | Warning | Install it (several at once when you select several imports) |
| `` `pkg` is not installed, and this project has no virtual environment yet. `` | Warning | Create the environment on Python X.Y, then install |
| `` `pkg` has no wheels for Python X.Y. It supports A–B. `` | Error | Recreate the environment on a supported Python, reinstall, then install |
| `` The installed `pkg` V has no `attr`. It provides: … `` | Error | None: which version to pin is your call |
| `` PyPI has no project named `pkg`. … `` | Warning | None on purpose: installing a name PyPI does not know is how a typo becomes a supply-chain incident. <Badge type="tip" text="1.3" /> Only a real 404 from PyPI raises it; when PyPI cannot be reached, the import gets no diagnostic |
| `` `pkg`: … `` (no files for this platform) | Error | None |
| CUDA extension problems (see above) | Error / Warning | None: a plain install is what would fail |

When the import name differs from the distribution (`cv2` → `opencv-python`), the quick fix title names both, because you are approving a name you never typed. In pixi projects the fixes run through `pixi add --pypi` and `pixi add python=…`, and their titles say so.

Attribute checks only fire when PyPilot can be sure the attribute is absent: a module with a dynamic `__getattr__` is left alone.
