#!/usr/bin/env python3
"""Generate helper/data/conda_forge_packages.json.

The table backs PyPilot's "pixi can help here" advisory: PyPI packages whose
source build needs system libraries (or nvcc) that pip/uv cannot install, and
the conda-forge build that ships those libraries.

The curated part is the PACKAGES list below: which PyPI projects are worth
naming, the system libraries their source build needs, and why that is
painful. Everything else is looked up:

  conda-forge name  parselmouth's PyPI -> conda mapping
                    (https://github.com/prefix-dev/parselmouth), unless
                    PACKAGES overrides it
  version, builds   prefix.dev's GraphQL API (https://prefix.dev/api/graphql):
                    the newest conda-forge version, the platforms it is built
                    for and, per platform, the CPython minors of its builds

LIBRARIES maps what a failed build names (a missing header, a `*-config`
tool, a pkg-config module, a shared library) to the conda-forge package that
provides it. Every conda name in both lists is checked to exist.

Rows whose conda-forge package cannot be found are dropped (and reported).
Output is sorted by PyPI name and written with LF endings.

Usage: python scripts/gen_conda_forge.py [--dry-run]
Stdlib only, Python 3.9+.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime
import json
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "helper" / "data" / "conda_forge_packages.json"
GRAPHQL = "https://prefix.dev/api/graphql"
PARSELMOUTH = ("https://raw.githubusercontent.com/prefix-dev/parselmouth/main/"
               "files/mapping_as_grayskull.json")
USER_AGENT = "pypilot-gen-conda-forge (+https://github.com/pypilot-dev/pypilot)"
# The conda subdirs PyPilot runs on (win-arm64 has too few conda-forge builds).
PLATFORMS = ["linux-64", "linux-aarch64", "osx-64", "osx-arm64", "win-64"]

# (pypi name, conda override or None, system libraries (conda-forge names), why, flags)
# flags: "nvcc" = its source build compiles CUDA code and needs the CUDA toolkit.
PACKAGES: List[Tuple[str, Optional[str], List[str], str, str]] = [
    # --- geospatial -------------------------------------------------------
    ("gdal", None, ["libgdal", "proj", "geos"], "its Python bindings must be compiled against a system GDAL of exactly the same version (gdal-config), which pip cannot install", ""),
    ("fiona", None, ["libgdal"], "it builds against the GDAL C library and its gdal-config", ""),
    ("rasterio", None, ["libgdal"], "it builds against the GDAL C library and its gdal-config", ""),
    ("pyogrio", None, ["libgdal"], "it builds against the GDAL C library", ""),
    ("pyproj", None, ["proj"], "older releases build against a system PROJ of a matching version", ""),
    ("shapely", None, ["geos"], "older releases build against the GEOS C library (geos_c.h)", ""),
    ("pygeos", None, ["geos"], "it builds against the GEOS C library (geos_c.h)", ""),
    ("cartopy", None, ["proj", "geos"], "it needs PROJ and GEOS, and older releases build against both", ""),
    ("basemap", None, ["geos", "proj"], "it builds against GEOS and has no wheels for recent Pythons", ""),
    ("rtree", None, ["libspatialindex"], "it needs the libspatialindex C library", ""),
    ("netcdf4", None, ["libnetcdf", "hdf5"], "it builds against the netCDF-C and HDF5 libraries", ""),
    ("h5py", None, ["hdf5"], "it builds against the HDF5 library (and a parallel one for MPI builds)", ""),
    ("tables", "pytables", ["hdf5", "blosc"], "it builds against HDF5 and Blosc", ""),
    ("pdal", "python-pdal", ["pdal"], "it builds against the PDAL C++ library, which is not on PyPI", ""),
    ("pygrib", None, ["eccodes"], "it builds against the ecCodes library", ""),
    ("eccodes", "python-eccodes", ["eccodes"], "it needs the ecCodes C library at runtime", ""),
    ("cfgrib", None, ["eccodes"], "it needs the ecCodes C library at runtime", ""),
    ("cf-units", None, ["udunits2"], "it needs the UDUNITS-2 library", ""),
    ("scitools-iris", "iris", ["udunits2"], "it needs UDUNITS-2 through cf-units", ""),
    ("esmpy", None, ["esmf"], "it needs the ESMF Fortran library, which is not on PyPI", ""),
    ("xesmf", None, ["esmf"], "it needs ESMF through esmpy, which is not on PyPI", ""),
    ("wrf-python", None, ["libgfortran"], "its source build needs a Fortran compiler", ""),
    ("pyhdf", None, ["hdf4"], "it builds against the HDF4 library", ""),
    ("pyvips", None, ["libvips"], "it needs the libvips image library", ""),
    ("osmium", None, ["libosmium", "boost"], "it builds against libosmium and Boost", ""),
    ("pyspatialite", None, ["libspatialite"], "it builds against libspatialite", ""),
    ("pykdtree", None, ["llvm-openmp"], "its source build needs OpenMP", ""),
    ("pyresample", None, ["llvm-openmp"], "its source build needs OpenMP via pykdtree", ""),
    ("gmsh", None, ["libglu"], "its wheels need OpenGL/GLU libraries (libGLU.so.1) at runtime on Linux", ""),
    # --- chemistry, biology, physics -------------------------------------
    ("rdkit", None, ["boost", "cairo"], "older releases (rdkit-pypi) had no wheels for new Pythons and building needs Boost", ""),
    ("rdkit-pypi", "rdkit", ["boost", "cairo"], "it is the old name of rdkit on PyPI, with no wheels for Python 3.12+", ""),
    ("openmm", None, ["ocl-icd", "cuda-version"], "its PyPI wheels lag new Pythons and platforms, and building needs SWIG and a C++ toolchain", ""),
    ("pymol-open-source", "pymol-open-source", ["glew", "libpng", "freetype", "libxml2"], "it has no PyPI wheels; building needs GLEW, libpng, FreeType and libxml2", ""),
    ("openbabel", None, ["openbabel", "swig"], "it builds against the Open Babel C++ library and SWIG", ""),
    ("pyscf", None, ["libcint", "libxc"], "it has no Windows wheels; building needs libcint, libxc and BLAS", ""),
    ("pysam", None, ["htslib"], "it has no Windows wheels and builds against htslib", ""),
    ("cyvcf2", None, ["htslib"], "it builds against htslib", ""),
    ("pybedtools", None, ["bedtools"], "it needs the bedtools binaries", ""),
    ("pybigwig", None, ["libcurl"], "it builds against libcurl", ""),
    ("htseq", None, ["htslib"], "it builds against htslib via pysam", ""),
    ("vina", None, ["boost"], "it has no wheels for most platforms and builds against Boost", ""),
    ("mdtraj", None, ["zlib"], "its wheels lag new Pythons; building needs a C compiler and zlib", ""),
    ("mdanalysis", None, ["zlib"], "its wheels lag new Pythons and platforms", ""),
    ("gpaw", None, ["libxc", "openblas", "openmpi"], "it builds against libxc, BLAS/LAPACK and MPI", ""),
    ("healpy", None, ["cfitsio", "healpix_cxx"], "it has no Windows wheels; building needs cfitsio and HEALPix C++", ""),
    ("fitsio", None, ["cfitsio"], "it builds against cfitsio", ""),
    ("galsim", None, ["fftw", "eigen"], "it builds against FFTW and Eigen", ""),
    ("camb", None, ["libgfortran"], "its source build needs a Fortran compiler", ""),
    ("pymeep", "pymeep", ["meep", "mpb", "harminv"], "Meep's Python interface is not on PyPI at all", ""),
    ("phonopy", None, ["openblas"], "its source build needs a C compiler and BLAS", ""),
    ("pymatgen", None, ["openblas"], "its wheels lag new Pythons", ""),
    ("spglib", None, [], "its wheels lag new Pythons and platforms", ""),
    ("ase", None, [], "it is pure Python but its scientific stack is easier from conda-forge", ""),
    # --- graphics, GUI, imaging -------------------------------------------
    ("pycairo", None, ["cairo", "pkg-config"], "it builds against the cairo C library, found with pkg-config", ""),
    ("cairocffi", None, ["cairo"], "it loads the cairo C library at runtime", ""),
    ("pygobject", None, ["gobject-introspection", "glib", "cairo", "pkg-config"], "it builds against GObject Introspection, GLib and cairo, found with pkg-config", ""),
    ("pygraphviz", None, ["graphviz"], "it builds against Graphviz's C headers (graphviz/cgraph.h)", ""),
    ("graphviz", "python-graphviz", ["graphviz"], "it calls the Graphviz `dot` binary, which pip cannot install", ""),
    ("wxpython", None, ["gtk3"], "it has no Linux wheels on PyPI; building it compiles wxWidgets against GTK", ""),
    ("pyqt5", "pyqt", ["qt-main"], "its wheels lag new Pythons and some platforms (no Linux arm64)", ""),
    ("vtk", None, ["libgl", "tbb"], "its wheels lag new Pythons for months; building means compiling VTK itself", ""),
    ("mayavi", None, ["vtk"], "it builds against VTK, whose wheels lag new Pythons", ""),
    ("pyvista", None, ["vtk"], "it needs VTK, whose wheels lag new Pythons", ""),
    ("open3d", None, ["libgl"], "its wheels lag new Pythons and have no Linux arm64 or Windows arm64 builds", ""),
    ("pygame", None, ["sdl2", "sdl2_image", "sdl2_mixer", "sdl2_ttf"], "its wheels lag new Pythons; building needs SDL2 and its image, mixer and ttf libraries", ""),
    ("kivy", None, ["sdl2", "gstreamer"], "its wheels lag new Pythons; building needs SDL2", ""),
    ("glfw", "pyglfw", ["glfw"], "it loads the GLFW shared library", ""),
    ("moderngl", None, ["libgl"], "it needs OpenGL libraries (libGL.so.1) at runtime on Linux", ""),
    ("pyopengl-accelerate", None, ["libgl"], "its source build needs OpenGL headers", ""),
    ("opencv-python", "opencv", ["libgl", "ffmpeg"], "its Linux wheels need libGL.so.1 from the system (missing on servers and containers)", ""),
    ("opencv-contrib-python", "opencv", ["libgl", "ffmpeg"], "its Linux wheels need libGL.so.1 from the system (missing on servers and containers)", ""),
    ("dlib", None, ["cmake", "libblas", "libjpeg-turbo", "libpng"], "it has no wheels: every install compiles it with CMake and a C++ compiler, which often fails on Windows", ""),
    ("face-recognition", "face_recognition", ["dlib"], "it depends on dlib, which has no wheels and compiles with CMake", ""),
    ("weasyprint", None, ["pango", "cairo"], "it loads Pango and cairo at runtime, which pip cannot install", ""),
    ("wand", None, ["imagemagick"], "it loads the ImageMagick library at runtime", ""),
    ("pdf2image", None, ["poppler"], "it calls Poppler's binaries, which pip cannot install", ""),
    ("pdftotext", None, ["poppler"], "it builds against poppler-cpp", ""),
    ("pytesseract", None, ["tesseract"], "it calls the Tesseract binary, which pip cannot install", ""),
    ("tesserocr", None, ["tesseract", "leptonica"], "it builds against Tesseract and Leptonica", ""),
    ("pyzbar", None, ["zbar"], "it loads the zbar library at runtime", ""),
    ("pylibdmtx", None, ["libdmtx"], "it loads the libdmtx library at runtime", ""),
    ("rawpy", None, ["libraw"], "its wheels lag new Pythons; building needs LibRaw", ""),
    ("imagecodecs", None, ["libjpeg-turbo", "libtiff", "libwebp", "zstd"], "its wheels lag new Pythons; building needs dozens of codec libraries", ""),
    ("py3exiv2", None, ["exiv2", "boost"], "it builds against exiv2 and Boost.Python", ""),
    ("gphoto2", None, ["libgphoto2"], "it builds against libgphoto2", ""),
    ("pyembree", None, ["embree"], "it builds against Intel Embree", ""),
    ("pybullet", None, [], "it has few wheels; installing usually compiles the whole Bullet engine for many minutes", ""),
    ("box2d-py", None, ["swig"], "it has no wheels for recent Pythons and builds with SWIG", ""),
    ("box2d", "pybox2d", ["swig"], "it has no wheels for recent Pythons and builds with SWIG", ""),
    ("ghostscript", "python-ghostscript", ["ghostscript"], "it loads the Ghostscript library at runtime", ""),
    ("camelot-py", None, ["ghostscript"], "it needs Ghostscript, which pip cannot install", ""),
    ("python-vlc", None, ["vlc"], "it loads libvlc at runtime", ""),
    # --- audio, video -----------------------------------------------------
    ("pyaudio", None, ["portaudio"], "it builds against PortAudio (portaudio.h), with no Linux wheels", ""),
    ("sounddevice", "python-sounddevice", ["portaudio"], "it needs the PortAudio library at runtime on Linux", ""),
    ("pyalsaaudio", None, ["alsa-lib"], "it builds against ALSA (alsa/asoundlib.h)", ""),
    ("python-rtmidi", None, ["alsa-lib"], "it builds against ALSA or JACK on Linux", ""),
    ("jack-client", None, ["jack"], "it loads the JACK library at runtime", ""),
    ("pyliblo", None, ["liblo"], "it builds against liblo", ""),
    ("pyfftw", None, ["fftw"], "it builds against FFTW (fftw3.h)", ""),
    ("aubio", None, ["libsndfile", "fftw"], "it has no wheels and builds against libsndfile and FFTW", ""),
    ("essentia", None, ["fftw", "libsamplerate", "taglib"], "it has no Windows wheels and builds against FFTW, libsamplerate and TagLib", ""),
    ("pydub", None, ["ffmpeg"], "it calls the ffmpeg binary, which pip cannot install", ""),
    ("ffmpeg-python", None, ["ffmpeg"], "it calls the ffmpeg binary, which pip cannot install", ""),
    ("av", None, ["ffmpeg"], "its wheels lag new Pythons; building needs FFmpeg's development libraries", ""),
    ("pocketsphinx", None, ["sphinxbase"], "its source build needs SWIG and a C toolchain", ""),
    ("pyttsx3", None, ["espeak-ng"], "it needs eSpeak on Linux, which pip cannot install", ""),
    # --- databases, networking, crypto -----------------------------------
    ("psycopg2", None, ["libpq"], "it has no Linux or macOS wheels and builds against libpq (pg_config, libpq-fe.h)", ""),
    ("mysqlclient", None, ["mysql-libs", "pkg-config"], "it builds against the MySQL/MariaDB client library (mysql_config, mysql.h)", ""),
    ("pyodbc", None, ["unixodbc"], "it needs unixODBC (sql.h) on Linux and macOS", ""),
    ("python-ldap", None, ["openldap", "cyrus-sasl"], "it builds against OpenLDAP and Cyrus SASL (lber.h, sasl/sasl.h)", ""),
    ("pymssql", None, ["freetds"], "it builds against FreeTDS", ""),
    ("cx-oracle", "cx_oracle", ["oracle-instantclient"], "it needs the Oracle Instant Client libraries", ""),
    ("kerberos", "pykerberos", ["krb5"], "it builds against MIT Kerberos (krb5-config)", ""),
    ("gssapi", "python-gssapi", ["krb5"], "it builds against MIT Kerberos (krb5-config)", ""),
    ("xmlsec", "python-xmlsec", ["xmlsec", "libxml2", "pkg-config"], "it builds against libxmlsec1 and libxml2, found with pkg-config", ""),
    ("lxml", None, ["libxml2", "libxslt"], "its wheels lag new Pythons; building needs libxml2 and libxslt (xslt-config)", ""),
    ("pycurl", None, ["libcurl", "openssl"], "it builds against libcurl (curl-config) and OpenSSL", ""),
    ("m2crypto", None, ["openssl", "swig"], "it builds against OpenSSL with SWIG", ""),
    ("python-snappy", None, ["snappy"], "it builds against the Snappy library", ""),
    ("pylibmc", None, ["libmemcached"], "it builds against libmemcached", ""),
    ("uwsgi", None, ["openssl", "pcre2"], "it has no wheels and does not build on Windows", ""),
    ("python-magic", None, ["libmagic"], "it loads libmagic at runtime, which Windows and macOS do not ship", ""),
    ("pynacl", None, ["libsodium"], "its source build needs libsodium", ""),
    ("plyvel", None, ["leveldb"], "it builds against LevelDB", ""),
    ("pyicu", None, ["icu", "pkg-config"], "it has no wheels and builds against ICU (icu-config / pkg-config)", ""),
    ("pyenchant", None, ["enchant"], "it loads the Enchant library at runtime", ""),
    ("libarchive-c", "python-libarchive-c", ["libarchive"], "it loads libarchive at runtime", ""),
    ("pygit2", None, ["libgit2"], "its wheels lag new Pythons; building needs libgit2 of a matching version", ""),
    ("pycups", None, ["cups"], "it builds against CUPS (cups/cups.h)", ""),
    ("dbus-python", None, ["dbus", "glib", "pkg-config"], "it has no wheels and builds against D-Bus and GLib, found with pkg-config", ""),
    ("systemd-python", None, ["libsystemd"], "it builds against libsystemd", ""),
    ("evdev", "python-evdev", ["linux-headers"], "it builds against Linux kernel input headers", ""),
    ("pyusb", None, ["libusb"], "it loads libusb at runtime", ""),
    ("hidapi", "cython-hidapi", ["hidapi"], "it builds against hidapi and libusb", ""),
    ("pyscard", None, ["pcsc-lite"], "it builds against PC/SC Lite on Linux", ""),
    ("python-snap7", None, ["snap7"], "it loads the snap7 library", ""),
    ("jpype1", "jpype1", ["openjdk"], "it needs a Java JDK", ""),
    ("pyjnius", None, ["openjdk"], "it builds against a Java JDK", ""),
    ("rpy2", None, ["r-base"], "it builds against and embeds R, which pip cannot install", ""),
    ("pythonnet", None, ["mono"], "it needs Mono or .NET on Linux and macOS", ""),
    ("sqlcipher3", None, ["sqlcipher"], "it builds against SQLCipher", ""),
    ("pysqlite3", None, ["libsqlite"], "it builds against SQLite", ""),
    ("jq", None, ["jq"], "it has no wheels for some platforms and builds jq and Oniguruma from source", ""),
    ("ta-lib", "ta-lib", ["libta-lib"], "it builds against the TA-Lib C library, which no OS package manager ships by default", ""),
    ("pyarrow", None, ["libarrow"], "its wheels lag new Pythons; building means compiling Arrow C++", ""),
    ("confluent-kafka", "python-confluent-kafka", ["librdkafka"], "its wheels lag new Pythons; building needs librdkafka", ""),
    ("cffi", None, ["libffi"], "its source build needs libffi (ffi.h)", ""),
    ("cryptography", None, ["openssl"], "its source build needs OpenSSL and Rust", ""),
    # --- numerics, optimisation, ML --------------------------------------
    ("numpy", None, ["libblas", "liblapack"], "a source build needs BLAS/LAPACK and a C compiler; wheels lag new Pythons", ""),
    ("scipy", None, ["libblas", "liblapack", "libgfortran"], "a source build needs BLAS/LAPACK and a Fortran compiler; wheels lag new Pythons", ""),
    ("scikit-learn", None, ["llvm-openmp"], "a source build needs OpenMP; wheels lag new Pythons", ""),
    ("pandas", None, [], "a source build needs a C++ compiler; wheels lag new Pythons", ""),
    ("matplotlib", None, ["freetype", "libpng", "qhull"], "a source build needs FreeType, libpng and Qhull", ""),
    ("pillow", None, ["libjpeg-turbo", "zlib", "libtiff", "libwebp"], "a source build needs libjpeg and zlib (jpeglib.h)", ""),
    ("statsmodels", None, ["libblas"], "its wheels lag new Pythons", ""),
    ("scikit-image", None, [], "its wheels lag new Pythons", ""),
    ("pywavelets", None, [], "its wheels lag new Pythons", ""),
    ("numba", None, ["llvmlite"], "it is tied to llvmlite, whose wheels lag new Pythons; building needs LLVM", ""),
    ("llvmlite", None, ["llvmdev"], "its wheels lag new Pythons; building needs a specific LLVM", ""),
    ("cvxopt", None, ["libblas", "suitesparse", "glpk"], "a source build needs BLAS, SuiteSparse and GLPK", ""),
    ("scikit-sparse", None, ["suitesparse"], "it builds against SuiteSparse (cholmod.h)", ""),
    ("scikit-umfpack", None, ["suitesparse"], "it builds against SuiteSparse's UMFPACK", ""),
    ("gmpy2", None, ["gmp", "mpfr", "libmpc"], "a source build needs GMP, MPFR and MPC (gmp.h)", ""),
    ("python-flint", None, ["libflint"], "a source build needs FLINT", ""),
    ("cyipopt", None, ["ipopt"], "it builds against the Ipopt solver, which is not on PyPI", ""),
    ("cylp", None, ["coin-or-cbc"], "it builds against COIN-OR Cbc/Clp", ""),
    ("pygmo", None, ["pagmo", "boost"], "it has wheels for Linux only; elsewhere it needs pagmo2 and Boost", ""),
    ("pyscipopt", None, ["scip"], "older releases build against SCIP", ""),
    ("glpk", None, ["glpk"], "it builds against GLPK", ""),
    ("mpi4py", None, ["openmpi"], "it builds against an MPI implementation (mpi.h, mpicc)", ""),
    ("petsc4py", None, ["petsc"], "it builds against PETSc, which takes a long time to compile", ""),
    ("slepc4py", None, ["slepc", "petsc"], "it builds against SLEPc and PETSc", ""),
    ("horovod", None, ["openmpi", "cmake"], "it has no wheels and compiles against MPI, NCCL and your framework", "nvcc"),
    ("pystan", None, [], "it compiles Stan models with a C++ toolchain and does not support Windows", ""),
    ("cmdstanpy", None, ["cmdstan"], "it needs CmdStan and a C++ toolchain to compile models", ""),
    ("prophet", None, ["cmdstan"], "it compiles a Stan model with CmdStan at install", ""),
    ("pytensor", None, ["libblas"], "it compiles C code at runtime and needs a compiler and BLAS", ""),
    ("pymc", None, ["libblas"], "it compiles C code at runtime through PyTensor", ""),
    ("hnswlib", None, [], "it has no wheels and always compiles from source", ""),
    ("annoy", "python-annoy", [], "it has no wheels and always compiles from source", ""),
    ("fasttext", None, [], "it has no wheels and always compiles from source", ""),
    ("nmslib", None, [], "it has no wheels for recent Pythons", ""),
    ("faiss-gpu", "faiss-gpu", ["libfaiss"], "its PyPI wheels stopped at old Pythons and CUDA versions", ""),
    ("faiss-cpu", "faiss-cpu", ["libfaiss"], "its wheels lag new Pythons", ""),
    ("xgboost", None, ["llvm-openmp"], "on macOS it needs libomp at runtime (\"Library not loaded: libomp.dylib\")", ""),
    ("lightgbm", None, ["llvm-openmp"], "on macOS it needs libomp at runtime (\"Library not loaded: libomp.dylib\")", ""),
    ("sentencepiece", None, [], "its wheels lag new Pythons; building needs CMake", ""),
    ("pycocotools", None, [], "its wheels lag new Pythons", ""),
    ("lap", None, [], "it has no wheels for recent Pythons", ""),
    ("mujoco", None, [], "its wheels lag new Pythons", ""),
    ("pyopencl", None, ["ocl-icd"], "it needs an OpenCL ICD loader", ""),
    ("triton", None, [], "it has no Windows or macOS wheels", ""),
    # --- CUDA source builds (need nvcc) ----------------------------------
    ("flash-attn", None, ["cuda-nvcc"], "it publishes only an sdist that compiles CUDA kernels with nvcc", "nvcc"),
    ("xformers", None, ["cuda-nvcc"], "a source build compiles CUDA kernels with nvcc", "nvcc"),
    ("mamba-ssm", None, ["cuda-nvcc"], "it publishes only an sdist that compiles CUDA kernels with nvcc", "nvcc"),
    ("causal-conv1d", None, ["cuda-nvcc"], "it publishes only an sdist that compiles CUDA kernels with nvcc", "nvcc"),
    ("bitsandbytes", None, ["cuda-nvcc"], "a source build compiles CUDA kernels with nvcc", "nvcc"),
    ("deepspeed", None, ["cuda-nvcc"], "its fused ops compile with nvcc at install (DS_BUILD_OPS) or at first use", "nvcc"),
    ("pycuda", None, ["cuda-nvcc"], "it has no Linux wheels and builds against the CUDA toolkit", "nvcc"),
    ("cupy", None, ["cuda-nvcc"], "the plain `cupy` package compiles against the CUDA toolkit (the cupy-cudaXXx wheels do not)", "nvcc"),
    ("torch-scatter", "pytorch_scatter", ["cuda-nvcc"], "its PyPI release is an sdist that compiles CUDA kernels against your torch", "nvcc"),
    ("torch-sparse", "pytorch_sparse", ["cuda-nvcc"], "its PyPI release is an sdist that compiles CUDA kernels against your torch", "nvcc"),
    ("torch-cluster", "pytorch_cluster", ["cuda-nvcc"], "its PyPI release is an sdist that compiles CUDA kernels against your torch", "nvcc"),
    ("torch-spline-conv", "pytorch_spline_conv", ["cuda-nvcc"], "its PyPI release is an sdist that compiles CUDA kernels against your torch", "nvcc"),
    ("pytorch3d", None, ["cuda-nvcc"], "it has no PyPI wheels; building compiles CUDA kernels with nvcc", "nvcc"),
    ("mmcv", None, ["cuda-nvcc"], "its PyPI release is an sdist that compiles CUDA ops with nvcc", "nvcc"),
    ("nvdiffrast", None, ["cuda-nvcc"], "it compiles CUDA kernels with nvcc", "nvcc"),
    ("auto-gptq", None, ["cuda-nvcc"], "a source build compiles CUDA kernels with nvcc", "nvcc"),
    ("spconv", None, ["cuda-nvcc"], "a source build compiles CUDA kernels with nvcc", "nvcc"),
    ("gsplat", None, ["cuda-nvcc"], "it compiles CUDA kernels with nvcc at install or first use", "nvcc"),
]

# What a failed build names -> the conda-forge package that provides it.
# kind: header (from `fatal error: X: No such file`), tool (a missing `*-config`
# or build tool), pkg-config (a module pkg-config could not find), library (a
# shared library the loader could not open).
LIBRARIES: List[Tuple[str, str, str]] = [
    ("header", "gdal.h", "libgdal"), ("header", "cpl_conv.h", "libgdal"),
    ("header", "ogr_api.h", "libgdal"), ("header", "proj.h", "proj"),
    ("header", "proj_api.h", "proj"), ("header", "geos_c.h", "geos"),
    ("header", "spatialindex/capi/sidx_api.h", "libspatialindex"),
    ("header", "netcdf.h", "libnetcdf"), ("header", "hdf5.h", "hdf5"),
    ("header", "H5public.h", "hdf5"), ("header", "mfhdf.h", "hdf4"),
    ("header", "eccodes.h", "eccodes"), ("header", "grib_api.h", "eccodes"),
    ("header", "udunits2.h", "udunits2"), ("header", "vips/vips8", "libvips"),
    ("header", "graphviz/cgraph.h", "graphviz"), ("header", "cgraph.h", "graphviz"),
    ("header", "cairo.h", "cairo"), ("header", "cairo/cairo.h", "cairo"),
    ("header", "pango/pango.h", "pango"), ("header", "glib.h", "glib"),
    ("header", "girepository.h", "gobject-introspection"),
    ("header", "ffi.h", "libffi"), ("header", "portaudio.h", "portaudio"),
    ("header", "alsa/asoundlib.h", "alsa-lib"), ("header", "sndfile.h", "libsndfile"),
    ("header", "fftw3.h", "fftw"), ("header", "jack/jack.h", "jack"),
    ("header", "libpq-fe.h", "libpq"), ("header", "pg_config.h", "libpq"),
    ("header", "mysql.h", "mysql-libs"), ("header", "mysql/mysql.h", "mysql-libs"),
    ("header", "sql.h", "unixodbc"), ("header", "sqlext.h", "unixodbc"),
    ("header", "lber.h", "openldap"), ("header", "ldap.h", "openldap"),
    ("header", "sasl/sasl.h", "cyrus-sasl"), ("header", "sybfront.h", "freetds"),
    ("header", "krb5.h", "krb5"), ("header", "gssapi/gssapi.h", "krb5"),
    ("header", "xmlsec/xmlsec.h", "xmlsec"), ("header", "libxml/xmlversion.h", "libxml2"),
    ("header", "libxml/parser.h", "libxml2"), ("header", "libxslt/xsltconfig.h", "libxslt"),
    ("header", "curl/curl.h", "libcurl"), ("header", "openssl/ssl.h", "openssl"),
    ("header", "openssl/opensslv.h", "openssl"), ("header", "openssl/aes.h", "openssl"),
    ("header", "snappy-c.h", "snappy"), ("header", "libmemcached/memcached.h", "libmemcached"),
    ("header", "magic.h", "libmagic"), ("header", "sodium.h", "libsodium"),
    ("header", "leveldb/db.h", "leveldb"), ("header", "unicode/utypes.h", "icu"),
    ("header", "git2.h", "libgit2"), ("header", "cups/cups.h", "cups"),
    ("header", "dbus/dbus.h", "dbus"), ("header", "systemd/sd-journal.h", "libsystemd"),
    ("header", "libusb.h", "libusb"), ("header", "libusb-1.0/libusb.h", "libusb"),
    ("header", "hidapi.h", "hidapi"), ("header", "winscard.h", "pcsc-lite"),
    ("header", "PCSC/winscard.h", "pcsc-lite"), ("header", "jni.h", "openjdk"),
    ("header", "sqlite3.h", "libsqlite"), ("header", "sqlcipher/sqlite3.h", "sqlcipher"),
    ("header", "ta-lib/ta_libc.h", "libta-lib"), ("header", "ta_libc.h", "libta-lib"),
    ("header", "arrow/api.h", "libarrow"), ("header", "librdkafka/rdkafka.h", "librdkafka"),
    ("header", "gmp.h", "gmp"), ("header", "mpfr.h", "mpfr"), ("header", "mpc.h", "libmpc"),
    ("header", "flint/flint.h", "libflint"), ("header", "cholmod.h", "suitesparse"),
    ("header", "suitesparse/cholmod.h", "suitesparse"), ("header", "umfpack.h", "suitesparse"),
    ("header", "glpk.h", "glpk"), ("header", "IpStdCInterface.h", "ipopt"),
    ("header", "coin/IpStdCInterface.h", "ipopt"), ("header", "mpi.h", "openmpi"),
    ("header", "petsc.h", "petsc"), ("header", "cblas.h", "libcblas"),
    ("header", "lapacke.h", "liblapacke"), ("header", "omp.h", "llvm-openmp"),
    ("header", "zlib.h", "zlib"), ("header", "bzlib.h", "bzip2"), ("header", "lzma.h", "xz"),
    ("header", "zstd.h", "zstd"), ("header", "lz4.h", "lz4-c"), ("header", "jpeglib.h", "libjpeg-turbo"),
    ("header", "png.h", "libpng"), ("header", "tiffio.h", "libtiff"), ("header", "webp/decode.h", "libwebp"),
    ("header", "ft2build.h", "freetype"), ("header", "libraw/libraw.h", "libraw"),
    ("header", "exiv2/exiv2.hpp", "exiv2"), ("header", "gphoto2/gphoto2.h", "libgphoto2"),
    ("header", "embree3/rtcore.h", "embree"), ("header", "tesseract/baseapi.h", "tesseract"),
    ("header", "leptonica/allheaders.h", "leptonica"), ("header", "zbar.h", "zbar"),
    ("header", "poppler/cpp/poppler-document.h", "poppler"), ("header", "poppler-document.h", "poppler"),
    ("header", "MagickWand/MagickWand.h", "imagemagick"), ("header", "SDL.h", "sdl2"),
    ("header", "SDL2/SDL.h", "sdl2"), ("header", "GL/gl.h", "libgl-devel"),
    ("header", "GL/glu.h", "libglu"), ("header", "GL/glew.h", "glew"), ("header", "X11/Xlib.h", "xorg-libx11"),
    ("header", "boost/python.hpp", "libboost-python-devel"), ("header", "boost/version.hpp", "libboost-devel"),
    ("header", "Eigen/Core", "eigen"), ("header", "eigen3/Eigen/Core", "eigen"),
    ("header", "htslib/sam.h", "htslib"), ("header", "htslib/hts.h", "htslib"),
    ("header", "fitsio.h", "cfitsio"), ("header", "xc.h", "libxc"),
    ("header", "libavcodec/avcodec.h", "ffmpeg"), ("header", "libavformat/avformat.h", "ffmpeg"),
    ("header", "CL/cl.h", "ocl-icd"), ("header", "Python.h", "python"),
    ("header", "cuda_runtime.h", "cuda-toolkit"), ("header", "cuda.h", "cuda-toolkit"),
    ("header", "cuda_runtime_api.h", "cuda-toolkit"), ("header", "cublas_v2.h", "cuda-toolkit"),
    ("header", "cusparse.h", "cuda-toolkit"), ("header", "nccl.h", "nccl"),
    ("header", "linux/input.h", "linux-headers"),
    ("tool", "gdal-config", "libgdal"), ("tool", "pg_config", "libpq"),
    ("tool", "mysql_config", "mysql-libs"), ("tool", "mariadb_config", "mysql-libs"),
    ("tool", "pkg-config", "pkg-config"), ("tool", "krb5-config", "krb5"),
    ("tool", "curl-config", "libcurl"), ("tool", "xslt-config", "libxslt"),
    ("tool", "icu-config", "icu"), ("tool", "geos-config", "geos"),
    ("tool", "nc-config", "libnetcdf"), ("tool", "h5cc", "hdf5"),
    ("tool", "mpicc", "openmpi"), ("tool", "swig", "swig"), ("tool", "cmake", "cmake"),
    ("tool", "ninja", "ninja"), ("tool", "gfortran", "gfortran"), ("tool", "nvcc", "cuda-nvcc"),
    ("tool", "llvm-config", "llvmdev"), ("tool", "ta-lib-config", "libta-lib"),
    ("tool", "dot", "graphviz"), ("tool", "ffmpeg", "ffmpeg"), ("tool", "tesseract", "tesseract"),
    ("tool", "pdftoppm", "poppler"), ("tool", "java", "openjdk"), ("tool", "R", "r-base"),
    ("pkg-config", "cairo", "cairo"), ("pkg-config", "gobject-introspection-1.0", "gobject-introspection"),
    ("pkg-config", "glib-2.0", "glib"), ("pkg-config", "dbus-1", "dbus"),
    ("pkg-config", "libffi", "libffi"), ("pkg-config", "xmlsec1", "xmlsec"),
    ("pkg-config", "libxml-2.0", "libxml2"), ("pkg-config", "mysqlclient", "mysql-libs"),
    ("pkg-config", "libmariadb", "mysql-libs"), ("pkg-config", "icu-i18n", "icu"),
    ("pkg-config", "icu-uc", "icu"), ("pkg-config", "libpq", "libpq"),
    ("pkg-config", "hdf5", "hdf5"), ("pkg-config", "netcdf", "libnetcdf"),
    ("pkg-config", "proj", "proj"), ("pkg-config", "gdal", "libgdal"),
    ("pkg-config", "libgit2", "libgit2"), ("pkg-config", "libsystemd", "libsystemd"),
    ("pkg-config", "libavformat", "ffmpeg"), ("pkg-config", "libavcodec", "ffmpeg"),
    ("pkg-config", "portaudio-2.0", "portaudio"), ("pkg-config", "sdl2", "sdl2"),
    ("pkg-config", "openblas", "openblas"), ("pkg-config", "lapack", "liblapack"),
    ("pkg-config", "libcurl", "libcurl"), ("pkg-config", "openssl", "openssl"),
    ("pkg-config", "libsodium", "libsodium"), ("pkg-config", "tesseract", "tesseract"),
    ("pkg-config", "lept", "leptonica"), ("pkg-config", "poppler-cpp", "poppler"),
    ("pkg-config", "libusb-1.0", "libusb"), ("pkg-config", "hidapi-libusb", "hidapi"),
    ("pkg-config", "libpcsclite", "pcsc-lite"), ("pkg-config", "fftw3", "fftw"),
    ("library", "libGL.so.1", "libgl"), ("library", "libGLU.so.1", "libglu"),
    ("library", "libgthread-2.0.so.0", "glib"), ("library", "libglib-2.0.so.0", "glib"),
    ("library", "libSM.so.6", "xorg-libsm"), ("library", "libXext.so.6", "xorg-libxext"),
    ("library", "libXrender.so.1", "xorg-libxrender"), ("library", "libX11.so.6", "xorg-libx11"),
    ("library", "libmagic", "libmagic"), ("library", "libcairo", "cairo"),
    ("library", "libgdal", "libgdal"), ("library", "libgeos_c", "geos"),
    ("library", "libspatialindex_c", "libspatialindex"), ("library", "libpq", "libpq"),
    ("library", "libomp.dylib", "llvm-openmp"), ("library", "libgomp.so.1", "libgomp"),
    ("library", "libportaudio", "portaudio"), ("library", "libsndfile", "libsndfile"),
    ("library", "libzbar", "zbar"), ("library", "libvips", "libvips"),
    ("library", "libpango", "pango"), ("library", "libgobject-2.0", "glib"),
    ("library", "libta_lib", "libta-lib"), ("library", "libcudart.so", "cuda-cudart"),
    ("library", "libnvrtc.so", "cuda-nvrtc"), ("library", "libcublas.so", "libcublas"),
    ("library", "libsodium", "libsodium"), ("library", "libusb-1.0", "libusb"),
]

# --------------------------------------------------------------------------


def http(url: str, body: Optional[bytes] = None) -> Any:
    headers = {"User-Agent": USER_AGENT}
    if body is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=body, headers=headers)
    for attempt in range(3):
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except (urllib.error.URLError, OSError, ValueError):
            if attempt == 2:
                raise
    raise RuntimeError("unreachable")


def graphql(query: str) -> Dict:
    data = http(GRAPHQL, json.dumps({"query": query}).encode())
    if data.get("errors"):
        raise RuntimeError(f"GraphQL error: {data['errors']}")
    return data["data"]


def gql_str(s: str) -> str:
    return json.dumps(s)


def latest_many(names: List[str]) -> Dict[str, Optional[Dict]]:
    """name -> {version, platforms} (None when conda-forge has no such package).
    Batched with aliases, 40 names per request."""
    out: Dict[str, Optional[Dict]] = {}
    for i in range(0, len(names), 40):
        chunk = names[i:i + 40]
        fields = " ".join(
            f'p{j}: package(channelName: "conda-forge", name: {gql_str(n)}) '
            f"{{ name latestVersion {{ version platforms }} }}"
            for j, n in enumerate(chunk))
        data = graphql("{ " + fields + " }")
        for j, n in enumerate(chunk):
            pkg = data.get(f"p{j}")
            out[n] = pkg["latestVersion"] if pkg else None
    return out


_PY_ABI = re.compile(r"^python_abi\s+3\.(\d+)\.\*\s+\*_cp3\d+(t?)$")
_PY_PIN = re.compile(r"^python\s+>=3\.(\d+)(?:\.\d+)?,<3\.(\d+)")
_PY_BUILD = re.compile(r"(?:^|_)py3(\d{1,2})(t?)(?:h|_|$)")


def python_minor(depends: List[str], build: str) -> Optional[int]:
    """The CPython minor a conda build targets; None for noarch/abi3/free-threaded."""
    for dep in depends:
        m = _PY_ABI.match(dep.strip())
        if m:
            return None if m.group(2) else int(m.group(1))
    for dep in depends:
        m = _PY_PIN.match(dep.strip().replace(" ", " "))
        if m and int(m.group(2)) == int(m.group(1)) + 1:
            return int(m.group(1))
    m = _PY_BUILD.search(build)
    if m and not m.group(2):
        return int(m.group(1))
    return None


def builds_for(conda: str, version: str, platforms: List[str]) -> List[Dict]:
    """Per tracked platform, the CPython minors conda-forge builds `version` for."""
    per: Dict[str, set] = {}
    noarch = False
    page = 0
    while True:
        q = (f'{{ package(channelName: "conda-forge", name: {gql_str(conda)}) '
             f'{{ variants(version: {gql_str(version)}, limit: 50, page: {page}) '
             f'{{ pages page {{ platform buildString rawIndex }} }} }} }}')
        res = graphql(q)["package"]["variants"]
        for v in res["page"]:
            plat = v["platform"]
            if plat == "noarch":
                noarch = True
                continue
            if plat not in PLATFORMS:
                continue
            per.setdefault(plat, set())
            minor = python_minor(v["rawIndex"].get("depends", []), v["buildString"])
            if minor is not None:
                per[plat].add(minor)
        page += 1
        if page >= res["pages"]:
            break
    if noarch and not per:
        return [{"platform": "noarch", "python": []}]
    rows = []
    for plat in PLATFORMS:
        if plat in per or plat in platforms:
            rows.append({"platform": plat,
                         "python": [f"3.{m}" for m in sorted(per.get(plat, set()))]})
    return rows


def normalize(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--dry-run", action="store_true", help="print, do not write")
    args = parser.parse_args()

    mapping = {normalize(k): v for k, v in http(PARSELMOUTH).items()}

    candidates: Dict[str, List[str]] = {}
    for pypi, override, _libs, _why, _flags in PACKAGES:
        names = [n for n in (override, mapping.get(normalize(pypi)), normalize(pypi)) if n]
        candidates[pypi] = list(dict.fromkeys(names))
    lib_names = sorted({c for _k, _n, c in LIBRARIES}
                       | {lib for _p, _o, libs, _w, _f in PACKAGES for lib in libs})
    all_names = sorted({n for ns in candidates.values() for n in ns} | set(lib_names))
    latest = latest_many(all_names)

    missing_libs = [n for n in lib_names if latest.get(n) is None]
    resolved: List[Tuple[str, str, Dict, Tuple]] = []
    dropped = []
    for row in PACKAGES:
        conda = next((n for n in candidates[row[0]] if latest.get(n)), None)
        if conda is None:
            dropped.append(row[0])
        else:
            resolved.append((row[0], conda, latest[conda], row))

    def work(item):
        pypi, conda, info, row = item
        return pypi, conda, info, row, builds_for(conda, info["version"], info["platforms"])

    rows = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        for pypi, conda, info, (_, _o, libs, why, flags), builds in pool.map(work, resolved):
            entry: Dict[str, Any] = {
                "pypi": pypi,
                "conda": conda,
                "version": info["version"],
                "builds": builds,
                "system_libs": [lib for lib in libs if latest.get(lib)],
                "why": why,
            }
            if "nvcc" in flags:
                entry["needs_nvcc"] = True
            rows.append(entry)
    rows.sort(key=lambda r: r["pypi"])

    libraries = [{"kind": k, "name": n, "conda": c}
                 for k, n, c in LIBRARIES if latest.get(c)]

    table = {
        "_comment": (
            "PyPI packages whose source build needs system libraries (or nvcc) pip and uv "
            "cannot install, and the conda-forge package that ships them; backs the \"pixi can "
            "help here\" advisory. `builds` lists, per conda platform, the CPython minors "
            "conda-forge builds `version` for (`noarch` = any). `libraries` maps what a failed "
            "build names (kind: header, tool, pkg-config, library) to the conda-forge package "
            "providing it. Generated by scripts/gen_conda_forge.py from prefix.dev "
            "(https://prefix.dev/api/graphql) and parselmouth "
            "(https://github.com/prefix-dev/parselmouth); edit the script, not this file."),
        "schema_version": 1,
        "generated": datetime.date.today().isoformat(),
        "packages": rows,
        "libraries": libraries,
    }
    text = json.dumps(table, indent=1, ensure_ascii=False) + "\n"
    if dropped:
        print(f"dropped (not on conda-forge): {', '.join(dropped)}", file=sys.stderr)
    if missing_libs:
        print(f"library names not on conda-forge (omitted): {', '.join(missing_libs)}",
              file=sys.stderr)
    print(f"{len(rows)} packages, {len(libraries)} library mappings", file=sys.stderr)
    if args.dry_run:
        print(text)
    else:
        OUT.write_bytes(text.encode("utf-8"))
        print(f"wrote {OUT.relative_to(ROOT)}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
