import Foundation
import XCTest
@testable import NulangUIProtocol

final class StrictDecodingTests: XCTestCase {
    func testRevisionRejectsJSONNumberInsteadOfLosslessString() {
        let json = Data("1".utf8)
        XCTAssertThrowsError(try JSONDecoder().decode(Revision.self, from: json))
    }

    func testI64RejectsJSONNumberInsteadOfLosslessString() {
        let json = Data("{\"type\":\"i64\",\"value\":9223372036854775807}".utf8)
        XCTAssertThrowsError(try JSONDecoder().decode(WireValue.self, from: json))
    }

    func testF64RequiresExactlySixteenHexDigits() {
        let short = Data("{\"type\":\"f64\",\"value\":\"3ff0\"}".utf8)
        XCTAssertThrowsError(try JSONDecoder().decode(WireValue.self, from: short))

        let invalid = Data("{\"type\":\"f64\",\"value\":\"zzzzzzzzzzzzzzzz\"}".utf8)
        XCTAssertThrowsError(try JSONDecoder().decode(WireValue.self, from: invalid))
    }

    func testRevisionRejectsNegativeZeroAndNegativeValues() {
        for json in ["\"-0\"", "\"-1\""] {
            XCTAssertThrowsError(try JSONDecoder().decode(Revision.self, from: Data(json.utf8)))
        }
    }

    func testRevisionAllowsPositiveSignLikeRustUnsignedParsing() throws {
        let revision = try JSONDecoder().decode(Revision.self, from: Data("\"+42\"".utf8))
        XCTAssertEqual(revision, Revision(42))
    }

    func testNodeRejectsExplicitNullCollections() {
        for field in ["properties", "children", "actions"] {
            let json = "{\"id\":\"title\",\"kind\":\"text\",\"\(field)\":null}"
            XCTAssertThrowsError(
                try JSONDecoder().decode(UiNode.self, from: Data(json.utf8)),
                "explicit null for \(field) must not decode as a missing collection"
            )
        }
    }

    func testNodeOmittedCollectionsDecodeToEmpty() throws {
        let json = Data("{\"id\":\"title\",\"kind\":\"text\"}".utf8)
        let node = try JSONDecoder().decode(UiNode.self, from: json)
        XCTAssertEqual(node.properties, [:])
        XCTAssertEqual(node.children, [])
        XCTAssertEqual(node.actions, [])
    }
}
