{ lib
, clangStdenv
, fetchFromGitHub
, autoreconfHook
, swift-corelibs-libdispatch
, clang
, llvm
, xar ? null
, tapi ? null
, targetTriple ? "x86_64-apple-darwin"
, ... }:

let
  _commit = "e5dfc5633cb9060a94d16b8d78a01eb0b3620021";
  repo = fetchFromGitHub {
    owner = "tpoechtrager";
    repo = "cctools-port";
    rev = _commit;
    hash = "sha256-yYWi1+Vu/GZ8IuNqL49wbfM+gXX5QYUo16dAMq9qMAc="; # 用真实 sha256 替换或用 nix-prefetch-git 预取
  };
in
clangStdenv.mkDerivation rec {
  pname = "cctools";
  _cctools_ver = "1030.6.3";
  version = "${_cctools_ver}+g${builtins.substring 0 7 _commit}";

  src = "${repo}/cctools";

  # 让 Nix 自动运行 autoreconf（如果需要）
  nativeBuildInputs = [
    autoreconfHook
  ];

  buildInputs = [
    llvm
    clang
    swift-corelibs-libdispatch
  ];

  # 传递给 configure 的参数（使用 $out 作为 prefix）
  configureFlags = [
    "--prefix=${placeholder "out"}"
    "--target=${targetTriple}"
    "--with-llvm-config=${llvm}/bin/llvm-config"
    "--libexecdir=${placeholder "out"}/bin"
  ];

  enableParallelBuilding = true;

  meta = with lib; {
    description = "Apple's cross-compiling toolchain ported to Linux";
    homepage = "https://github.com/tpoechtrager/cctools-port";
    license = with lib.licenses; [
      apple-psl20
      gpl2 # GNU as
    ];
  };
}
