# GPU & CUDA

For pip-installed PyTorch and TensorFlow, your **NVIDIA driver** decides which builds work, not the CUDA toolkit installed on the system. The wheels bundle their own CUDA runtime, and drivers are backward compatible, so the question is: what is the newest CUDA runtime this driver supports, and which build of the framework targets it?

PyPilot answers that automatically whenever a project declares `torch` or `tensorflow`.

## What it does

1. Runs `nvidia-smi` to read the GPU model and driver version.
2. Maps the driver to the newest CUDA runtime it supports (NVIDIA's minimum-driver table, Linux and Windows) and the GPU architecture to the CUDA range it needs (an Ampere `sm_86` card needs a build against CUDA 11.1 or newer, for example).
3. Picks the newest PyTorch build that fits both, such as `cu124` or `cu132`, and installs torch from that index on `setup`, `install` and **Fix environment**. You do not choose the build yourself.

<TerminalDemo src="doctor-gpu" cmd="pypilot doctor" title="~/src/gpu" />

| Machine | Result |
|---|---|
| NVIDIA GPU | The matching `cuXXX` build, reported as an Info finding |
| No GPU | The plain CPU build, silently |
| Apple Silicon | An Info note that torch/tensorflow use Metal (MPS); the default wheel is correct |
| Driver too old for any build the framework ships | A finding naming the gap |

## Keeping the build in uv projects

`uv add --index …` steers one resolution only; the next `uv sync` re-resolves from `pyproject.toml` and quietly swaps the CUDA build for the PyPI one ([astral-sh/uv#17186](https://github.com/astral-sh/uv/issues/17186)). So in a uv project PyPilot writes the index into the manifest, as uv's PyTorch guide recommends: a named, `explicit` `[[tool.uv.index]]` and a `[tool.uv.sources]` entry for each torch-family package (torch, torchvision, torchaudio…), limited to Linux and Windows so macOS keeps the PyPI build. Your comments and layout in `pyproject.toml` are preserved.

## `pypilot fix cuda`

Re-pins `torch` and `tensorflow` to the build for the current driver, for example after a driver upgrade. In a uv project it updates the index in `pyproject.toml` and syncs. pixi projects are left alone (pixi's own CUDA packages apply there). See [the command reference](/engine/commands#fix-cuda).

## TensorFlow on Windows

TensorFlow 2.10 was the last release with GPU support on native Windows. For newer releases PyPilot reports that tensorflow will run on the CPU and lists the ways out: WSL2 with `tensorflow[and-cuda]`; `tensorflow<2.11` on Python 3.10 or older with CUDA 11.2 and cuDNN 8.1 (`conda install -c conda-forge cudatoolkit=11.2 cudnn=8.1.0`); or PyTorch, which ships CUDA builds for Windows. With `tensorflow==2.10.0` it reports what that release needs instead.

## CUDA extensions

`flash-attn`, `xformers`, `bitsandbytes`, `mamba-ssm` and `causal-conv1d` are compiled against specific torch versions. PyPilot knows each release's wheel platforms and torch pin, and warns when:

- the project's torch pin does not match what the extension's wheels need (`… needs torch==…`): the install will fail to resolve or pull a different torch;
- installing the latest release would replace the installed torch, often with a build for another CUDA;
- an extension has no prebuilt wheel here, so it compiles from source and needs `nvcc`. Then the [pixi advisory](/pixi/advisory) may suggest conda-forge's build.

## Where the data lives

| Table | Holds |
|---|---|
| `nvidia.json` | CUDA version → minimum driver (Linux, Windows); GPU architecture → CUDA/torch range |
| `frameworks.json` | torch releases and their `cuXXX` builds; tensorflow releases and their minimum CUDA |
| `cuda_extensions.json` | flash-attn, xformers, bitsandbytes, mamba-ssm, causal-conv1d: wheel platforms and torch pin per release |

They refresh on installed engines every week, so a new torch or driver release is picked up without upgrading PyPilot. See [Data tables](/reference/data-tables).
