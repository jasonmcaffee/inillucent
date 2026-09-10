//! `geopoly`: polygons as values, ported from `ext/rtree/geopoly.c`.
//!
//! Invariant: **a polygon is the same bytes here as it is in SQLite**, and
//! every measure over one answers the same number. The on-disk form is a file
//! format - a table written by one engine is read by the other - so a shape
//! whose blob differed by a byte, or an area that differed in the sixth digit,
//! would be a silent divergence in stored data rather than a difference in an
//! answer.
//!
//! A polygon is a closed ring of at least three distinct vertexes, stored
//! without the repeated closing vertex:
//!
//! | bytes | meaning |
//! |---|---|
//! | 0 | `1` for little-endian coordinates, `0` for big-endian |
//! | 1..3 | how many vertexes, big-endian |
//! | 4.. | `x` then `y` per vertex, `f32` in the header's byte order |
//!
//! The JSON form is `[[x,y],...]` **with** the closing vertex repeated, which
//! is GeoJSON's rule and is why a round trip through `geopoly_json` gains a
//! vertex and `geopoly_blob` loses it again.
//!
//! Two ports here are exact rather than equivalent, and both are worth naming.
//! `sine` is the reference's own fifth-order polynomial approximation, not
//! `f64::sin`: `geopoly_regular(0,0,10,4)` answers `10.0007` in SQLite, and a
//! more accurate sine would answer `10` - a *better* number and a different
//! file. And `overlap` reproduces the sweep's tie-breaking, including that its
//! event sort puts the later of two events at the same `x` first, because the
//! order two segments enter the active list in decides which mask bits the
//! sweep sets.

use inillucent_value::Value;

/// A closed polygon, without the repeated closing vertex.
#[derive(Clone, Debug, PartialEq)]
pub struct Polygon {
    /// The vertexes, `x` then `y`, in winding order.
    pub vertices: Vec<(f32, f32)>,
}

/// The smallest blob a polygon can be: a header and three vertexes.
const SMALLEST: usize = 4 + 6 * 4;

/// How many vertexes `geopoly_regular` will make.
const MOST_SIDES: i64 = 1000;

/// The reference's own value of pi, to the digit.
///
/// Spelled out rather than `std::f64::consts::PI` because it is the reference
/// implementation's constant, and `geopoly_regular` has to place its vertexes
/// where SQLite's does to the last bit. The two happen to agree today; naming
/// the standard one would make that agreement an assumption rather than a
/// transcription.
#[allow(clippy::approx_constant)]
const PI: f64 = 3.141_592_653_589_793;

impl Polygon {
    /// Returns the polygon a value holds, when it holds one.
    ///
    /// A blob is read as the binary form and text as GeoJSON; anything else is
    /// not a polygon. This is `geopolyFuncParam`, and the two shapes are the
    /// two a column may hold, because `geopoly_blob` is what a table stores and
    /// a person writes JSON.
    ///
    /// @param value - the argument to interpret
    pub fn parse(value: Option<&Value<'static>>) -> Option<Polygon> {
        match value {
            Some(Value::Blob(blob)) => Polygon::from_blob(blob.raw()),
            Some(Value::Text(text)) => Polygon::from_json(text.raw()),
            _ => None,
        }
    }

    /// Returns the polygon a stored value holds, or nothing, or a refusal.
    ///
    /// **Three answers rather than two, and the reference's own three.** A
    /// `geopoly` table indexes the extent of the shape in its `_shape` column,
    /// so what happens to a row whose `_shape` is *not* a polygon is a rule the
    /// format has to state, and SQLite's is subtler than "refuse it":
    ///
    /// - text that does not begin with `[` was never claiming to be GeoJSON, so
    ///   it is **stored** with an empty box and is simply never found by a
    ///   spatial query - which is what lets a column hold a placeholder;
    /// - text that begins with `[` and does not parse is a **refusal**, because
    ///   something meant it as a polygon and got it wrong;
    /// - a blob long enough to be a polygon but not one is stored, and a blob
    ///   too short to be one is a refusal;
    /// - and anything that is not text or a blob is a refusal.
    ///
    /// `Ok(None)` is the stored-with-an-empty-box case.
    ///
    /// @param value - the column's value
    #[allow(clippy::result_unit_err)]
    pub fn parse_for_index(value: Option<&Value<'static>>) -> Result<Option<Polygon>, ()> {
        match value {
            Some(Value::Blob(blob)) if blob.raw().len() >= SMALLEST => {
                Ok(Polygon::from_blob(blob.raw()))
            }
            Some(Value::Text(text)) => {
                let raw = text.raw();
                if let Some(shape) = Polygon::from_json(raw) {
                    return Ok(Some(shape));
                }
                let opens = raw
                    .iter()
                    .find(|byte| !byte.is_ascii_whitespace())
                    .is_some_and(|byte| *byte == b'[');
                if opens {
                    Err(())
                } else {
                    Ok(None)
                }
            }
            _ => Err(()),
        }
    }

    /// Returns the polygon a blob holds.
    ///
    /// @param bytes - the stored form
    pub fn from_blob(bytes: &[u8]) -> Option<Polygon> {
        if bytes.len() < SMALLEST {
            return None;
        }
        let endian = *bytes.first()?;
        if endian != 0 && endian != 1 {
            return None;
        }
        let count =
            (usize::from(bytes[1]) << 16) | (usize::from(bytes[2]) << 8) | usize::from(bytes[3]);
        if count.saturating_mul(8).saturating_add(4) != bytes.len() {
            return None;
        }
        let mut vertices = Vec::with_capacity(count);
        for index in 0..count {
            let at = 4 + index * 8;
            let x = coordinate(&bytes[at..at + 4], endian)?;
            let y = coordinate(&bytes[at + 4..at + 8], endian)?;
            vertices.push((x, y));
        }
        Some(Polygon { vertices })
    }

    /// Returns the polygon a GeoJSON array holds.
    ///
    /// The ring has to close - the last vertex equal to the first, bit for bit
    /// - and hold at least four vertexes counting that repeat, which is three
    /// distinct ones. The repeat is dropped on the way in.
    ///
    /// @param text - the JSON source
    pub fn from_json(text: &[u8]) -> Option<Polygon> {
        let mut scan = Scan { text, at: 0 };
        if scan.skip_space() != Some(b'[') {
            return None;
        }
        scan.at += 1;
        let mut flat: Vec<f32> = Vec::new();
        while scan.skip_space() == Some(b'[') {
            scan.at += 1;
            let mut seen = 0usize;
            while let Some(number) = scan.number() {
                if seen < 2 {
                    flat.push(number as f32);
                }
                seen += 1;
                let closing = scan.skip_space();
                scan.at += 1;
                match closing {
                    Some(b',') => continue,
                    Some(b']') if seen >= 2 => break,
                    _ => return None,
                }
            }
            if seen < 2 {
                return None;
            }
            if scan.skip_space() == Some(b',') {
                scan.at += 1;
                continue;
            }
            break;
        }
        let count = flat.len() / 2;
        if scan.skip_space() != Some(b']') || count < 4 {
            return None;
        }
        scan.at += 1;
        if scan.skip_space().is_some() {
            return None;
        }
        if flat[0] != flat[count * 2 - 2] || flat[1] != flat[count * 2 - 1] {
            return None;
        }
        let vertices = (0..count - 1)
            .map(|at| (flat[at * 2], flat[at * 2 + 1]))
            .collect();
        Some(Polygon { vertices })
    }

    /// Returns the stored form.
    pub fn to_blob(&self) -> Vec<u8> {
        let count = self.vertices.len();
        let mut bytes = Vec::with_capacity(4 + count * 8);
        bytes.push(1);
        bytes.push(((count >> 16) & 0xff) as u8);
        bytes.push(((count >> 8) & 0xff) as u8);
        bytes.push((count & 0xff) as u8);
        for (x, y) in &self.vertices {
            bytes.extend_from_slice(&x.to_le_bytes());
            bytes.extend_from_slice(&y.to_le_bytes());
        }
        bytes
    }

    /// Returns the GeoJSON form, with the ring closed again.
    pub fn to_json(&self) -> String {
        let mut out = String::from("[");
        for (x, y) in &self.vertices {
            out.push_str(&format!(
                "[{},{}],",
                general(f64::from(*x)),
                general(f64::from(*y))
            ));
        }
        let (x, y) = self.vertices.first().copied().unwrap_or((0.0, 0.0));
        out.push_str(&format!(
            "[{},{}]]",
            general(f64::from(x)),
            general(f64::from(y))
        ));
        out
    }

    /// Returns an SVG `<polyline>` for the shape.
    ///
    /// Every argument after the first is written into the tag as it stands,
    /// which is how the reference lets a caller colour a shape without this
    /// having to know anything about SVG.
    ///
    /// @param attributes - the extra arguments, already rendered as text
    pub fn to_svg(&self, attributes: &[String]) -> String {
        let mut out = String::from("<polyline points=");
        let mut separator = '\'';
        for (x, y) in &self.vertices {
            out.push_str(&format!(
                "{separator}{},{}",
                plain(f64::from(*x)),
                plain(f64::from(*y))
            ));
            separator = ' ';
        }
        let (x, y) = self.vertices.first().copied().unwrap_or((0.0, 0.0));
        out.push_str(&format!(
            " {},{}'",
            plain(f64::from(x)),
            plain(f64::from(y))
        ));
        for attribute in attributes {
            if !attribute.is_empty() {
                out.push(' ');
                out.push_str(attribute);
            }
        }
        out.push_str("></polyline>");
        out
    }

    /// Returns the enclosed area, negative when the ring winds clockwise.
    ///
    /// The shoelace sum, and the sign is the useful half: RFC 7946 says a ring
    /// winds counter-clockwise, so a negative area is how `geopoly_ccw` knows
    /// it has been handed a shape that needs reversing.
    pub fn area(&self) -> f64 {
        let count = self.vertices.len();
        if count == 0 {
            return 0.0;
        }
        let mut total = 0.0f64;
        for at in 0..count - 1 {
            let (x0, y0) = self.vertices[at];
            let (x1, y1) = self.vertices[at + 1];
            total += (f64::from(x0) - f64::from(x1)) * (f64::from(y0) + f64::from(y1)) * 0.5;
        }
        let (xn, yn) = self.vertices[count - 1];
        let (x0, y0) = self.vertices[0];
        total += (f64::from(xn) - f64::from(x0)) * (f64::from(yn) + f64::from(y0)) * 0.5;
        total
    }

    /// Returns the same ring wound counter-clockwise.
    ///
    /// The first vertex stays where it is and the rest are reversed, which
    /// keeps the shape's starting point rather than only its winding.
    pub fn counter_clockwise(&self) -> Polygon {
        let mut vertices = self.vertices.clone();
        if self.area() < 0.0 && vertices.len() > 2 {
            vertices[1..].reverse();
        }
        Polygon { vertices }
    }

    /// Returns the shape's bounding box: minimum and maximum on each axis.
    pub fn bounds(&self) -> [f32; 4] {
        let Some((first_x, first_y)) = self.vertices.first().copied() else {
            return [0.0; 4];
        };
        let (mut low_x, mut high_x, mut low_y, mut high_y) = (first_x, first_x, first_y, first_y);
        for (x, y) in self.vertices.iter().skip(1) {
            if *x < low_x {
                low_x = *x;
            } else if *x > high_x {
                high_x = *x;
            }
            if *y < low_y {
                low_y = *y;
            } else if *y > high_y {
                high_y = *y;
            }
        }
        [low_x, high_x, low_y, high_y]
    }

    /// Returns the transformed shape, `x1 = A*x + B*y + E`, `y1 = C*x + D*y + F`.
    ///
    /// @param matrix - `A`, `B`, `C`, `D`, `E`, `F`, in that order
    pub fn transformed(&self, matrix: [f64; 6]) -> Polygon {
        let [a, b, c, d, e, f] = matrix;
        let vertices = self
            .vertices
            .iter()
            .map(|(x, y)| {
                let (x0, y0) = (f64::from(*x), f64::from(*y));
                ((a * x0 + b * y0 + e) as f32, (c * x0 + d * y0 + f) as f32)
            })
            .collect();
        Polygon { vertices }
    }

    /// Returns where a point sits: 0 outside, 1 on the boundary, 2 inside.
    ///
    /// A crossing count, with the boundary answered before the count is
    /// finished - a point that lies *on* an edge is neither in nor out, and
    /// saying so is what makes the function usable for a join predicate.
    ///
    /// @param x - the point's first coordinate
    /// @param y - the point's second coordinate
    pub fn contains_point(&self, x: f64, y: f64) -> i64 {
        let count = self.vertices.len();
        if count == 0 {
            return 0;
        }
        let mut crossings = 0i64;
        let mut last = 0i64;
        let mut at = 0usize;
        while at + 1 < count {
            let (x1, y1) = self.vertices[at];
            let (x2, y2) = self.vertices[at + 1];
            last = beneath(
                x,
                y,
                f64::from(x1),
                f64::from(y1),
                f64::from(x2),
                f64::from(y2),
            );
            if last == 2 {
                break;
            }
            crossings += last;
            at += 1;
        }
        if last != 2 {
            let (x1, y1) = self.vertices[at.min(count - 1)];
            let (x2, y2) = self.vertices[0];
            last = beneath(
                x,
                y,
                f64::from(x1),
                f64::from(y1),
                f64::from(x2),
                f64::from(y2),
            );
        }
        if last == 2 {
            return 1;
        }
        if (last + crossings) % 2 == 0 {
            return 0;
        }
        2
    }
}

/// Returns the box four vertexes wide that a bounding box describes.
///
/// Counter-clockwise from the low corner, which is the winding the rest of the
/// module assumes and the winding SQLite writes.
///
/// @param bounds - minimum and maximum on each axis
pub fn box_polygon(bounds: [f32; 4]) -> Polygon {
    let [low_x, high_x, low_y, high_y] = bounds;
    Polygon {
        vertices: vec![
            (low_x, low_y),
            (high_x, low_y),
            (high_x, high_y),
            (low_x, high_y),
        ],
    }
}

/// Returns a regular polygon, or nothing when the shape is impossible.
///
/// **The angles come from the reference's own sine approximation.** A more
/// accurate one gives a *different file*: `geopoly_regular(0,0,10,4)` is
/// `10.0007` wide in SQLite and would be exactly `10` with `f64::sin`.
///
/// @param x - the centre's first coordinate
/// @param y - the centre's second coordinate
/// @param radius - the circumradius
/// @param sides - how many sides, capped at a thousand
pub fn regular(x: f64, y: f64, radius: f64, sides: i64) -> Option<Polygon> {
    if sides < 3 || radius <= 0.0 {
        return None;
    }
    let sides = sides.min(MOST_SIDES);
    let mut vertices = Vec::with_capacity(sides as usize);
    for step in 0..sides {
        let angle = 2.0 * PI * (step as f64) / (sides as f64);
        vertices.push((
            (x - radius * sine(angle - 0.5 * PI)) as f32,
            (y + radius * sine(angle)) as f32,
        ));
    }
    Some(Polygon { vertices })
}

/// Returns how two polygons meet.
///
/// | | |
/// |---|---|
/// | 0 | they are disjoint |
/// | 1 | they overlap |
/// | 2 | the first is inside the second |
/// | 3 | the second is inside the first |
/// | 4 | they are the same shape |
///
/// A sweep from left to right: every edge becomes two events, the active
/// edges are kept sorted by where they cross the sweep line, and a pair of
/// adjacent edges from *different* polygons that swap places between two
/// events have crossed - which settles the answer at 1 immediately. When
/// nothing crosses, the answer is read out of which gaps between adjacent
/// edges were inside which polygon.
///
/// @param first - one polygon
/// @param second - the other
pub fn overlap(first: &Polygon, second: &Polygon) -> i64 {
    let mut segments: Vec<Segment> = Vec::new();
    let mut events: Vec<Event> = Vec::new();
    add_segments(&mut segments, &mut events, first, 1);
    add_segments(&mut segments, &mut events, second, 2);
    // **Later first, on a tie.** The reference sorts its events with a merge
    // that prefers the right-hand list, and the right-hand list is always the
    // newer one - so two events at the same `x` come out in the reverse of the
    // order they were made. It decides the order two segments enter the active
    // list in, and that decides which gaps the sweep counts.
    let mut order: Vec<usize> = (0..events.len()).collect();
    order.sort_by(|left, right| {
        events[*left]
            .x
            .partial_cmp(&events[*right].x)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(right.cmp(left))
    });

    let mut active: Vec<usize> = Vec::new();
    let mut sweep = match order.first() {
        Some(first) if events[*first].x == 0.0 => -1.0,
        _ => 0.0,
    };
    let mut inside = [false; 4];
    let mut needs_sort = false;
    for position in order {
        let event = events[position];
        if event.x != sweep {
            sweep = event.x;
            if needs_sort {
                sort_active(&mut active, &segments);
                needs_sort = false;
            }
            let mut mask = 0usize;
            let mut previous: Option<usize> = None;
            for index in &active {
                if let Some(before) = previous {
                    if segments[before].y != segments[*index].y {
                        inside[mask] = true;
                    }
                }
                mask ^= usize::from(segments[*index].side);
                previous = Some(*index);
            }
            let mut mask = 0usize;
            let mut previous: Option<usize> = None;
            for index in &active {
                let at = *index;
                segments[at].y = segments[at].slope * sweep + segments[at].intercept;
                if let Some(before) = previous {
                    if segments[before].y > segments[at].y
                        && segments[before].side != segments[at].side
                    {
                        return 1;
                    }
                    if segments[before].y != segments[at].y {
                        inside[mask] = true;
                    }
                }
                mask ^= usize::from(segments[at].side);
                previous = Some(at);
            }
        }
        if event.removing {
            if let Some(at) = active.iter().position(|index| *index == event.segment) {
                active.remove(at);
            }
        } else {
            segments[event.segment].y = f64::from(segments[event.segment].start_y);
            active.insert(0, event.segment);
            needs_sort = true;
        }
    }
    if !inside[3] {
        return 0;
    }
    match (inside[1], inside[2]) {
        (true, false) => 3,
        (false, true) => 2,
        (false, false) => 4,
        (true, true) => 1,
    }
}

/// One edge of one polygon, as the sweep sees it.
#[derive(Clone, Copy, Debug)]
struct Segment {
    /// The `C` of `y = C*x + B`.
    slope: f64,
    /// The `B` of `y = C*x + B`.
    intercept: f64,
    /// Where the edge crosses the sweep line right now.
    y: f64,
    /// Where it crossed when it entered.
    start_y: f32,
    /// `1` for the first polygon, `2` for the second.
    side: u8,
}

/// An edge entering or leaving the sweep.
#[derive(Clone, Copy, Debug)]
struct Event {
    /// Where on the sweep line it happens.
    x: f64,
    /// Whether the edge is leaving rather than entering.
    removing: bool,
    /// Which edge.
    segment: usize,
}

/// Adds every edge of one polygon to the sweep.
///
/// @param segments - the edge list being built
/// @param events - the event list being built
/// @param polygon - the shape to take edges from
/// @param side - `1` or `2`, which polygon this is
fn add_segments(segments: &mut Vec<Segment>, events: &mut Vec<Event>, polygon: &Polygon, side: u8) {
    let count = polygon.vertices.len();
    if count == 0 {
        return;
    }
    for at in 0..count - 1 {
        let (x0, y0) = polygon.vertices[at];
        let (x1, y1) = polygon.vertices[at + 1];
        add_one_segment(segments, events, x0, y0, x1, y1, side);
    }
    let (x0, y0) = polygon.vertices[count - 1];
    let (x1, y1) = polygon.vertices[0];
    add_one_segment(segments, events, x0, y0, x1, y1, side);
}

/// Adds one edge, left endpoint first, ignoring the vertical ones.
///
/// A vertical edge has no slope and contributes nothing the sweep can use: it
/// is entered and left at the same `x`, so it never separates two other edges.
///
/// @param segments - the edge list being built
/// @param events - the event list being built
/// @param x0 - the first endpoint's first coordinate
/// @param y0 - the first endpoint's second coordinate
/// @param x1 - the second endpoint's first coordinate
/// @param y1 - the second endpoint's second coordinate
/// @param side - `1` or `2`, which polygon this is
#[allow(clippy::too_many_arguments)]
fn add_one_segment(
    segments: &mut Vec<Segment>,
    events: &mut Vec<Event>,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    side: u8,
) {
    if x0 == x1 {
        return;
    }
    let (x0, y0, x1, y1) = if x0 > x1 {
        (x1, y1, x0, y0)
    } else {
        (x0, y0, x1, y1)
    };
    let slope = (f64::from(y1) - f64::from(y0)) / (f64::from(x1) - f64::from(x0));
    let at = segments.len();
    segments.push(Segment {
        slope,
        intercept: f64::from(y1) - f64::from(x1) * slope,
        y: 0.0,
        start_y: y0,
        side,
    });
    events.push(Event {
        x: f64::from(x0),
        removing: false,
        segment: at,
    });
    events.push(Event {
        x: f64::from(x1),
        removing: true,
        segment: at,
    });
}

/// Sorts the active edges by where they cross the sweep line, then by slope.
///
/// Stable, because the reference's merge keeps the earlier of two edges that
/// tie on both - and which of two identical edges comes first decides which
/// gap the mask attributes to which polygon.
///
/// @param active - the edges currently crossing the sweep line
/// @param segments - every edge
fn sort_active(active: &mut [usize], segments: &[Segment]) {
    active.sort_by(|left, right| {
        let one = &segments[*left];
        let two = &segments[*right];
        one.y
            .partial_cmp(&two.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                one.slope
                    .partial_cmp(&two.slope)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
}

/// Returns 2 when a point is on a segment, 1 when it is below it, 0 otherwise.
///
/// The left endpoint is deliberately *not* part of the segment, which is what
/// stops a ray through a vertex from being counted twice.
///
/// @param x - the point's first coordinate
/// @param y - the point's second coordinate
/// @param x1 - the segment's first endpoint
/// @param y1 - the segment's first endpoint
/// @param x2 - the segment's second endpoint
/// @param y2 - the segment's second endpoint
fn beneath(x: f64, y: f64, x1: f64, y1: f64, x2: f64, y2: f64) -> i64 {
    if x == x1 && y == y1 {
        return 2;
    }
    if x1 < x2 {
        if x <= x1 || x > x2 {
            return 0;
        }
    } else if x1 > x2 {
        if x <= x2 || x > x1 {
            return 0;
        }
    } else {
        if x != x1 {
            return 0;
        }
        if y < y1 && y < y2 {
            return 0;
        }
        if y > y1 && y > y2 {
            return 0;
        }
        return 2;
    }
    let crossing = y1 + (y2 - y1) * (x - x1) / (x2 - x1);
    if y == crossing {
        return 2;
    }
    if y < crossing {
        return 1;
    }
    0
}

/// Returns the reference's own sine approximation.
///
/// Valid from `-pi/2` to `2*pi`, which is the whole range `regular` asks for.
///
/// @param angle - the angle in radians
fn sine(angle: f64) -> f64 {
    let mut angle = angle;
    if angle >= 1.5 * PI {
        angle -= 2.0 * PI;
    }
    if angle >= 0.5 * PI {
        return -sine(angle - PI);
    }
    let square = angle * angle;
    let cube = square * angle;
    let fifth = cube * square;
    0.9996949 * angle - 0.1656700 * cube + 0.0075134 * fifth
}

/// Returns one `f32` out of a blob, in the header's byte order.
///
/// @param bytes - the four bytes
/// @param endian - the header's first byte
fn coordinate(bytes: &[u8], endian: u8) -> Option<f32> {
    let raw: [u8; 4] = bytes.try_into().ok()?;
    Some(if endian == 1 {
        f32::from_le_bytes(raw)
    } else {
        f32::from_be_bytes(raw)
    })
}

/// Returns `%g` of a number, which is what the SVG form writes.
///
/// @param value - the number
fn plain(value: f64) -> String {
    crate::printf::general(value)
}

/// Returns `%!g` of a number, which is what the JSON form writes.
///
/// The `!` is the reference's own flag for "keep the decimal point", so a
/// whole number reads `0.0` rather than `0` - which is what makes the JSON a
/// round trip rather than something a stricter reader would take as an
/// integer.
///
/// @param value - the number
fn general(value: f64) -> String {
    let text = crate::printf::general(value);
    if text.contains(['.', 'e', 'E', 'n', 'i']) {
        return text;
    }
    format!("{text}.0")
}

/// A cursor over GeoJSON source.
struct Scan<'a> {
    text: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    /// Returns the next byte that is not whitespace, without consuming it.
    fn skip_space(&mut self) -> Option<u8> {
        while matches!(
            self.text.get(self.at),
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(0x0b) | Some(0x0c)
        ) {
            self.at += 1;
        }
        self.text.get(self.at).copied()
    }

    /// Reads one JSON number, or nothing when the next token is not one.
    ///
    /// The reference's own scanner: a leading zero may not be followed by a
    /// digit, a decimal point may not follow the sign, and an exponent has to
    /// have a digit after it.
    fn number(&mut self) -> Option<f64> {
        self.skip_space();
        let start = self.at;
        let text = self.text;
        let mut at = start;
        if text.get(at) == Some(&b'-') {
            at += 1;
        }
        if text.get(at) == Some(&b'0') && text.get(at + 1).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        let mut seen_point = false;
        let mut seen_exponent = false;
        loop {
            let byte = text.get(at).copied().unwrap_or(0);
            if byte.is_ascii_digit() {
                at += 1;
                continue;
            }
            if byte == b'.' {
                if at == start || text.get(at - 1) == Some(&b'-') || seen_point {
                    return None;
                }
                seen_point = true;
                at += 1;
                continue;
            }
            if byte == b'e' || byte == b'E' {
                if at == start || !text.get(at - 1).is_some_and(u8::is_ascii_digit) {
                    return None;
                }
                if seen_exponent {
                    return None;
                }
                seen_point = true;
                seen_exponent = true;
                at += 1;
                if matches!(text.get(at), Some(b'+') | Some(b'-')) {
                    at += 1;
                }
                if !text.get(at).is_some_and(u8::is_ascii_digit) {
                    return None;
                }
                continue;
            }
            break;
        }
        if at == start || !text.get(at - 1).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        let parsed = std::str::from_utf8(&text[start..at]).ok()?.parse().ok()?;
        self.at = at;
        Some(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blob is the file format, so the bytes are checked against the ones
    /// the reference writes for the same JSON rather than against themselves.
    #[test]
    fn a_square_round_trips_through_both_forms() {
        let square = Polygon::from_json(b"[[0,0],[3,0],[3,3],[0,3],[0,0]]").expect("it parses");
        assert_eq!(square.vertices.len(), 4);
        let blob = square.to_blob();
        assert_eq!(
            blob.iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>(),
            "010000040000000000000000000040400000000000004040000040400000000000004040"
        );
        assert_eq!(Polygon::from_blob(&blob), Some(square.clone()));
        assert_eq!(
            square.to_json(),
            "[[0.0,0.0],[3.0,0.0],[3.0,3.0],[0.0,3.0],[0.0,0.0]]"
        );
    }

    /// The sign of the area is the winding, which is what `geopoly_ccw` reads.
    #[test]
    fn winding_shows_in_the_sign_of_the_area() {
        let counter = Polygon::from_json(b"[[0,0],[3,0],[3,3],[0,3],[0,0]]").expect("it parses");
        let clockwise = Polygon::from_json(b"[[0,0],[0,3],[3,3],[3,0],[0,0]]").expect("it parses");
        assert_eq!(counter.area(), 9.0);
        assert_eq!(clockwise.area(), -9.0);
        assert_eq!(clockwise.counter_clockwise().to_json(), counter.to_json());
    }

    /// The reference's own sine, which is why this is `10.0007` and not `10`.
    #[test]
    fn a_regular_polygon_matches_the_references_approximation() {
        let square = regular(0.0, 0.0, 10.0, 4).expect("four sides is a polygon");
        assert_eq!(
            square.to_json(),
            "[[10.0007,0.0],[0.0,10.0007],[-10.0007,0.0],[0.0,-10.0007],[10.0007,0.0]]"
        );
        assert_eq!(regular(0.0, 0.0, 10.0, 2), None);
        assert_eq!(regular(0.0, 0.0, 0.0, 4), None);
    }

    /// The five answers the sweep can give, one shape pair each.
    #[test]
    fn the_sweep_names_every_way_two_shapes_can_meet() {
        let outer = Polygon::from_json(b"[[0,0],[3,0],[3,3],[0,3],[0,0]]").expect("it parses");
        let inner = Polygon::from_json(b"[[1,1],[2,1],[2,2],[1,2],[1,1]]").expect("it parses");
        let apart = Polygon::from_json(b"[[9,9],[10,9],[10,10],[9,9]]").expect("it parses");
        let across = Polygon::from_json(b"[[2,2],[5,2],[5,5],[2,5],[2,2]]").expect("it parses");
        assert_eq!(overlap(&outer, &inner), 3);
        assert_eq!(overlap(&inner, &outer), 2);
        assert_eq!(overlap(&outer, &apart), 0);
        assert_eq!(overlap(&outer, &outer), 4);
        assert_eq!(overlap(&outer, &across), 1);
    }

    /// Inside, on the boundary and outside are three answers rather than two.
    #[test]
    fn a_point_is_in_out_or_on_the_edge() {
        let square = Polygon::from_json(b"[[0,0],[3,0],[3,3],[0,3],[0,0]]").expect("it parses");
        assert_eq!(square.contains_point(1.0, 1.0), 2);
        assert_eq!(square.contains_point(0.0, 0.0), 1);
        assert_eq!(square.contains_point(9.0, 9.0), 0);
    }

    /// A ring that does not close, and one that is not a ring at all.
    #[test]
    fn what_is_not_a_polygon_is_refused_rather_than_half_read() {
        assert_eq!(Polygon::from_json(b"[[0,0],[3,0]]"), None);
        assert_eq!(Polygon::from_json(b"nope"), None);
        assert_eq!(Polygon::from_json(b"[[0,0],[3,0],[3,3],[0,3]]"), None);
        assert_eq!(Polygon::from_blob(&[1, 0, 0, 4]), None);
    }
}
