'use strict';

// Maps the current Node runtime to the Rust target triple whose prebuilt
// archive the release pipeline publishes, plus the names of the binaries that
// live inside that archive. Shared by the postinstall (which downloads) and the
// bin shim (which resolves the installed binary), so the two can never disagree.

// Node `process.platform` -> the OS/ABI portion of the Rust target triple.
const OS_MAP = {
  linux: 'unknown-linux-gnu',
  darwin: 'apple-darwin',
  win32: 'pc-windows-msvc',
};

// Node `process.arch` -> the CPU portion of the Rust target triple.
const ARCH_MAP = {
  x64: 'x86_64',
  arm64: 'aarch64',
};

// Exactly the targets the release pipeline builds. A combination that maps
// through OS_MAP/ARCH_MAP but is not in this set (e.g. aarch64 windows) is
// still unsupported and must fail loudly.
const SUPPORTED_TARGETS = [
  'x86_64-unknown-linux-gnu',
  'aarch64-unknown-linux-gnu',
  'x86_64-apple-darwin',
  'aarch64-apple-darwin',
  'x86_64-pc-windows-msvc',
];

class UnsupportedPlatformError extends Error {
  constructor(message) {
    super(message);
    this.name = 'UnsupportedPlatformError';
  }
}

function detect(nodePlatform, nodeArch) {
  const platform = nodePlatform || process.platform;
  const arch = nodeArch || process.arch;

  const osPart = OS_MAP[platform];
  const archPart = ARCH_MAP[arch];
  const isWindows = platform === 'win32';

  if (!osPart || !archPart) {
    throw new UnsupportedPlatformError(
      `no prebuilt agit binary for platform "${platform}" / arch "${arch}".`
    );
  }

  const target = `${archPart}-${osPart}`;
  if (!SUPPORTED_TARGETS.includes(target)) {
    throw new UnsupportedPlatformError(
      `no prebuilt agit binary for "${platform}/${arch}" (target ${target}).`
    );
  }

  const primaryBinary = isWindows ? 'agit.exe' : 'agit';
  const hubBinary = isWindows ? 'agit-hub.exe' : 'agit-hub';

  return {
    platform,
    arch,
    target,
    isWindows,
    ext: isWindows ? 'zip' : 'tar.gz',
    primaryBinary,
    hubBinary,
    binaries: [primaryBinary, hubBinary],
  };
}

// e.g. archiveName('0.1.0', 'x86_64-unknown-linux-gnu', 'tar.gz')
//      -> 'agit-0.1.0-x86_64-unknown-linux-gnu.tar.gz'
function archiveName(version, target, ext) {
  return `agit-${version}-${target}.${ext}`;
}

function supportedList() {
  return SUPPORTED_TARGETS.join(', ');
}

module.exports = {
  detect,
  archiveName,
  supportedList,
  UnsupportedPlatformError,
  SUPPORTED_TARGETS,
  OS_MAP,
  ARCH_MAP,
};
