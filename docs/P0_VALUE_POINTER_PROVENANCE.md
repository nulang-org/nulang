# P0 Value pointer provenance

Tracking note for the implementation that closes #186.

This branch makes raw pointer-to-`Value` construction an explicit unsafe boundary and adds regression coverage so generic safe Rust cannot fabricate pointer-tagged values.
