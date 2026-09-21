# @nulang/geo

Experimental first-party geospatial foundations for Nulang.

The package is intentionally implemented as user-space Nulang rather than as
language syntax. It tests whether Nulang's general-purpose type system and
runtime are sufficient for safe, high-performance domain libraries without
expanding the language kernel.

## Current scope

- CRS-tagged `Point[CRS]` and `BoundingBox[CRS]` values
- marker types for WGS84 (CRS84-style longitude/latitude storage), Web Mercator, and local Cartesian coordinates
- WGS84 range validation and longitude normalization
- axis-aligned bounding-box containment/intersection
- planar Euclidean distance
- typed `Distance` values with meter/kilometer/mile conversion
- spherical WGS84 great-circle distance
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
`contains`, `intersects`, and `distance_2d` require compatible instantiated
point/bounds types.

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

Those integrations should be split into optional packages/components so the
core `nulang-geo` package remains dependency-light and portable.

## Testing

From this package directory:

```bash
nulang nula test
```

The initial suite covers CRS-tagged point construction, bounds predicates,
coordinate validation, unit conversion, planar distance, and an equatorial
great-circle reference distance.
