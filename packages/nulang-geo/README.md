# @nulang/geo

Experimental first-party geospatial foundations for Nulang.

The package is intentionally implemented as user-space Nulang rather than as
language syntax. It tests whether Nulang's general-purpose type system and
runtime are sufficient for safe, high-performance domain libraries without
expanding the language kernel.

## Current scope

- CRS-tagged `Point[CRS]`, `BoundingBox[CRS]`, `LineString[CRS]`, and `Polygon[CRS]` values
- marker types for WGS84 (CRS84-style longitude/latitude storage), Web Mercator, and local Cartesian coordinates
- WGS84 range validation and longitude normalization
- axis-aligned bounding-box containment/intersection
- planar Euclidean distance, line length, polygon perimeter, area, centroid, and envelopes
- typed `Distance` values with meter/kilometer/mile conversion
- spherical WGS84 great-circle distance
- WKT writers for points, line strings, and single-ring polygons
- dependency-free immutable `RTree[CRS, T]` bulk loading and intersection queries
- leaf-level STR packing, point queries, contained-by queries, and traversal statistics
- dependency-free implementation using portable Nulang float primitives

## Usage

```nulang
import lib

fn main() {
    let office = wgs84(-47.8828, -15.7939)
    let area = bounding_box(-48.1, -16.0, -47.7, -15.6, WGS84)

    if contains(area, office) then {
        perform IO.print("inside")
    } else {
        perform IO.print("outside")
    }
}
```

`wgs84(longitude, latitude)` always stores `x=longitude` and `y=latitude`.
This matches OGC:CRS84 ordering rather than EPSG:4326's formal latitude-first
axis order.

CRS parameters are shared across spatial operations, so APIs such as
`contains`, `intersects`, `distance_2d`, `line_string`, and `polygon`
require compatible instantiated geometry types.

Planar operations such as `line_string_length`, `polygon_area`, and
`polygon_centroid` operate in raw CRS coordinate units. They are appropriate
for Cartesian/projected coordinates; they do not turn longitude/latitude
degrees into metric length or geodesic area. Use `geodesic_distance` for the
current WGS84 metric-distance path, and keep survey-grade geodesic geometry in
the future PROJ/GEOS-backed extensions.

## Design boundary

This package should remain a library. The language/runtime should only gain
general primitives when geospatial work exposes a broadly useful deficiency.

Planned extensions:

1. native PROJ transforms
2. GEOS predicates and topology
3. R-tree indexing
4. H3/S2 cell indexes
5. GeoArrow/GeoParquet
6. GDAL-backed vector/raster I/O
7. spatial stream and actor-partition helpers

The initial R-tree loader preserves input order and focuses on correctness, CRS safety, and pruning. `rtree_bulk_load_str` adds leaf-level Sort-Tile-Recursive-style packing while reusing the stable parent builder. Point and contained-by queries share the same CRS safety, and `rtree_query_intersects_with_stats` exposes nodes tested, entries tested, and matches for tuning and benchmarks. Mutable insert/delete operations remain follow-on work.

Those integrations should be split into optional packages/components so the
core `nulang-geo` package remains dependency-light and portable.

## Testing

From this package directory:

```bash
nulang nula test
```

The suite covers CRS-tagged point and aggregate construction, bounds and
envelopes, coordinate validation, unit conversion, planar distance/length/area/
centroid, WKT output, an equatorial great-circle reference distance, and
compile-fail regressions for cross-CRS operations, mixed-CRS line strings, cross-CRS R-tree box queries, and cross-CRS R-tree point queries. A reproducible `benches/rtree_query.nula` workload can be run with `nulang --bench N benches/rtree_query.nula` for before/after comparisons.
