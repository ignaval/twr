// swift-tools-version: 5.9
import PackageDescription

// Argon2id for typed phone pairing (docs/design/phone-pairing.md). `argon2/` is the
// reference implementation from https://github.com/P-H-C/phc-winner-argon2 at f57e61e
// (CC0 1.0 or Apache 2.0, see argon2/LICENSE), unmodified and reduced to the portable code
// Argon2id needs: no SIMD (opt.c), no threads (one lane), no tools or tests.
// ../../scripts/test-argon2.sh checks it against clients/shared/fixtures/phone-pairing-v2.json.
let package = Package(
    name: "TwrArgon2",
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "TwrArgon2", targets: ["TwrArgon2"])
    ],
    targets: [
        .target(
            name: "TwrArgon2",
            exclude: ["argon2/LICENSE"],
            cSettings: [
                .headerSearchPath("argon2/include"),
                .headerSearchPath("argon2/src"),
                .define("ARGON2_NO_THREADS")
            ]
        )
    ]
)
