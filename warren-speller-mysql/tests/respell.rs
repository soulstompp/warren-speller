// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Respelling against lego's catalog, as the server holds it: each rule's output written out
//! by hand, and the statements each rule leaves as they were read.

use warren_speller_mysql::catalog::Catalog;
use warren_speller_mysql::{respell, respell_query, sql};

// ---------------------------------------------------------------------------------------------
// A database's catalog, as the server holds it

const LEGO: &str = include_str!("catalogs/lego.tsv");

fn lego() -> Catalog {
    Catalog::parse(LEGO).expect("the catalog is refused")
}

/// The catalog with only the keys that are unique: every other index taken out.
fn lego_without_keys() -> Catalog {
    let kept: Vec<&str> = LEGO
        .lines()
        .filter(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            f[0] != "key" || f[4] == "0"
        })
        .collect();
    Catalog::parse(&kept.join("\n")).unwrap()
}

/// The statement respelled against lego's catalog.
fn on_lego(sql: &str) -> String {
    respell(&lego(), sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The statement is written back against lego's catalog exactly as it was read.
fn as_read_on_lego(sql: &str) {
    assert_eq!(on_lego(sql), sql, "the statement was respelled");
}

// ---------------------------------------------------------------------------------------------
// What is read, and how it is written back

#[test]
fn a_statement_ending_in_a_semicolon_is_written_with_one() {
    let once = on_lego(ZONES);
    assert_eq!(
        on_lego(&format!("{ZONES};\n")),
        format!("{};\n", once.trim_end())
    );
}

#[test]
fn a_statement_that_is_not_a_query_is_written_as_read() {
    as_read_on_lego(&format!("INSERT INTO zone_totals {ZONES}"));
}

#[test]
fn a_string_is_written_back_as_it_was_written() {
    assert_eq!(
        on_lego(
            r"SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id WHERE c.set_num = 'it''s \\ \'x\'' GROUP BY b.home_zone"
        ),
        "WITH side_a (k1, g1, n) AS (SELECT b.builder_id, b.home_zone, COUNT(*) FROM lego_builders b \
         GROUP BY b.builder_id, b.home_zone) \
         SELECT side_a.g1 AS home_zone, CAST(SUM(side_a.n * side_b.n) AS SIGNED) AS n \
         FROM side_a \
         JOIN (SELECT c.builder_id, COUNT(*) FROM lego_collection c \
         WHERE c.set_num = 'it''s \\\\ \\'x\\'' AND c.builder_id IN (SELECT k1 FROM side_a) GROUP BY c.builder_id) \
         AS side_b (k1, n) ON side_a.k1 = side_b.k1 GROUP BY side_a.g1\n"
    );
}

#[test]
fn a_parameter_is_never_copied_or_moved() {
    assert_eq!(
        on_lego(&format!("{ZONES} ORDER BY n DESC LIMIT ?")),
        format!("{} ORDER BY n DESC LIMIT ?\n", on_lego(ZONES).trim_end())
    );
}

#[test]
fn two_statements_or_an_unreadable_one_are_refused() {
    let why = respell(&lego(), "SELECT 1; SELECT 2").unwrap_err().0;
    assert!(
        why.contains("2 statements were read; respell reads one"),
        "{why}"
    );
    let why = respell(&lego(), "SELECT (").unwrap_err().0;
    assert!(why.contains("the statement could not be read"), "{why}");
}

// ---------------------------------------------------------------------------------------------
// A relation every UNION ALL arm joins

/// Each collection row with the version-1 inventory of its set and of each set nested in it.
const ARMS: &str = "SELECT c.builder_id, i.id AS inventory_id, 1 AS copies \
                    FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num AND i.version = 1 \
                    UNION ALL SELECT c.builder_id, ci.id, sub.quantity \
                    FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num AND i.version = 1 \
                    JOIN lego_inventory_sets sub ON sub.inventory_id = i.id \
                    JOIN lego_inventories ci ON ci.set_num = sub.set_num AND ci.version = 1";

const ARMS_JOINED_ONCE: &str = "SELECT c.builder_id, arms.c1 AS inventory_id, arms.c2 AS copies \
     FROM lego_collection c JOIN (SELECT i.set_num, i.id, 1 FROM lego_inventories i WHERE i.version = 1 \
     UNION ALL SELECT i.set_num, ci.id, sub.quantity FROM lego_inventories i \
     JOIN lego_inventory_sets sub ON sub.inventory_id = i.id \
     JOIN lego_inventories ci ON ci.set_num = sub.set_num AND ci.version = 1 WHERE i.version = 1) \
     AS arms (k1, c1, c2) ON c.set_num = arms.k1";

#[test]
fn a_relation_every_arm_joins_is_joined_once_to_the_union_of_the_rest() {
    assert_eq!(on_lego(ARMS), format!("{ARMS_JOINED_ONCE}\n"));
}

#[test]
fn a_union_in_a_with_query_or_a_derived_table_has_its_common_relation_taken_out() {
    assert_eq!(
        on_lego(&format!("WITH l AS ({ARMS}) SELECT builder_id FROM l")),
        format!("WITH l AS ({ARMS_JOINED_ONCE}) SELECT builder_id FROM l\n")
    );
    assert_eq!(
        on_lego(&format!("SELECT x.builder_id FROM ({ARMS}) AS x")),
        format!("SELECT x.builder_id FROM ({ARMS_JOINED_ONCE}) AS x\n")
    );
}

#[test]
fn a_condition_on_the_common_relation_alone_is_kept_on_it() {
    assert_eq!(
        on_lego(
            "SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
             WHERE c.row_no = 1 AND i.version = 1 \
             UNION ALL SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
             WHERE i.version = 2 AND c.row_no = 1"
        ),
        "SELECT c.builder_id, arms.c1 AS id FROM lego_collection c JOIN (SELECT i.set_num, i.id FROM lego_inventories i \
         WHERE i.version = 1 UNION ALL SELECT i.set_num, i.id FROM lego_inventories i WHERE i.version = 2) AS arms (k1, c1) \
         ON c.set_num = arms.k1 WHERE c.row_no = 1\n"
    );
}

#[test]
fn a_union_whose_arms_share_no_relation_is_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
         UNION ALL SELECT s.theme_id, sub.quantity FROM lego_sets s JOIN lego_inventory_sets sub ON sub.set_num = s.set_num",
    );
}

#[test]
fn a_union_with_an_arm_reading_the_relation_twice_is_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
         UNION ALL SELECT c.builder_id, d.row_no FROM lego_collection c JOIN lego_collection d ON d.set_num = c.set_num",
    );
}

#[test]
fn a_union_whose_arms_join_the_relation_on_other_columns_is_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
         UNION ALL SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.id = c.row_no",
    );
}

#[test]
fn a_union_whose_arms_join_the_relation_by_an_inequality_is_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num > c.set_num \
         UNION ALL SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num > c.set_num",
    );
}

#[test]
fn a_union_whose_arms_differ_in_conditions_on_the_relation_is_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
         WHERE c.row_no = 1 AND i.version = 1 \
         UNION ALL SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num",
    );
}

#[test]
fn a_union_whose_arms_join_by_an_outer_join_is_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id, i.id FROM lego_collection c LEFT JOIN lego_inventories i ON i.set_num = c.set_num \
         UNION ALL SELECT c.builder_id, i.id FROM lego_collection c LEFT JOIN lego_inventories i ON i.set_num = c.set_num",
    );
}

#[test]
fn a_union_returning_a_column_of_the_relation_in_one_arm_and_of_the_rest_in_another_is_left_alone()
{
    as_read_on_lego(
        "SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
         UNION ALL SELECT i.version, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num",
    );
}

#[test]
fn a_union_returning_a_column_of_both_is_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id + i.id AS x FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
         UNION ALL SELECT c.builder_id + i.id AS x FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num",
    );
}

#[test]
fn a_union_that_is_not_all_is_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
         UNION SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num",
    );
}

#[test]
fn a_union_with_an_order_or_a_limit_is_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
         UNION ALL SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
         ORDER BY 1 LIMIT 5",
    );
}

#[test]
fn a_union_naming_no_column_is_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id, i.id + 1 FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
         UNION ALL SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num",
    );
}

#[test]
fn arms_holding_a_parameter_are_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num AND i.version = ? \
         UNION ALL SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num",
    );
}

// ---------------------------------------------------------------------------------------------
// A grouped SELECT over a join, grouped on each side before the join

/// Each home zone's collection rows, their row numbers summed, and the least set they hold.
const ZONES: &str = "SELECT b.home_zone, COUNT(*) AS n, SUM(c.row_no) AS s, MIN(c.set_num) AS m \
                     FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id GROUP BY b.home_zone";

#[test]
fn a_grouped_join_is_grouped_on_each_side_before_the_join() {
    assert_eq!(
        on_lego(ZONES),
        "WITH side_a (k1, g1, n) AS (SELECT b.builder_id, b.home_zone, COUNT(*) FROM lego_builders b \
         GROUP BY b.builder_id, b.home_zone) \
         SELECT side_a.g1 AS home_zone, CAST(SUM(side_a.n * side_b.n) AS SIGNED) AS n, \
         SUM(side_b.p2 * side_a.n) AS s, MIN(side_b.p3) AS m \
         FROM side_a \
         JOIN (SELECT c.builder_id, COUNT(*), SUM(c.row_no), MIN(c.set_num) FROM lego_collection c \
         WHERE c.builder_id IN (SELECT k1 FROM side_a) GROUP BY c.builder_id) \
         AS side_b (k1, n, p2, p3) ON side_a.k1 = side_b.k1 GROUP BY side_a.g1\n"
    );
}

/// The collection rows of the first builders, with the lines of their sets and of the sets nested
/// in them, by home zone and colour: the rows counted and the bricks summed.
const OWNED: &str = "WITH owned AS (SELECT c.builder_id, c.row_no, c.set_num FROM lego_collection c WHERE c.builder_id < 50), \
     held AS (SELECT o.builder_id, o.row_no, o.set_num, i.id AS inventory_id, 1 AS copies \
     FROM owned o JOIN lego_inventories i ON i.set_num = o.set_num AND i.version = 1 \
     UNION ALL SELECT o.builder_id, o.row_no, o.set_num, ci.id, sub.quantity \
     FROM owned o JOIN lego_inventories i ON i.set_num = o.set_num AND i.version = 1 \
     JOIN lego_inventory_sets sub ON sub.inventory_id = i.id \
     JOIN lego_inventories ci ON ci.set_num = sub.set_num AND ci.version = 1) \
     SELECT b.home_zone, col.name AS colour, COUNT(DISTINCT l.builder_id, l.row_no) AS owned, \
     SUM(ip.quantity * l.copies) AS bricks \
     FROM held l JOIN lego_builders b ON b.builder_id = l.builder_id \
     JOIN lego_inventory_parts ip ON ip.inventory_id = l.inventory_id \
     JOIN lego_colors col ON col.id = ip.color_id \
     GROUP BY 1, 2 ORDER BY bricks DESC, 1, 2 LIMIT 10";

const OWNED_FOLDED: &str = "WITH owned AS (SELECT c.builder_id, c.row_no, c.set_num FROM lego_collection c WHERE c.builder_id < 50), \
     side_a (k1, g1, n) AS (SELECT o.set_num, b.home_zone, COUNT(*) FROM owned o \
     JOIN lego_builders b ON b.builder_id = o.builder_id GROUP BY o.set_num, b.home_zone) \
     SELECT side_a.g1 AS home_zone, side_b.g2 AS colour, CAST(SUM(side_a.n) AS SIGNED) AS owned, \
     SUM(side_b.p2 * side_a.n) AS bricks \
     FROM side_a \
     JOIN (SELECT arms.k1, col.name, SUM(ip.quantity * arms.c2) \
     FROM (SELECT i.set_num, i.id, 1 FROM lego_inventories i WHERE i.version = 1 \
     UNION ALL SELECT i.set_num, ci.id, sub.quantity FROM lego_inventories i \
     JOIN lego_inventory_sets sub ON sub.inventory_id = i.id \
     JOIN lego_inventories ci ON ci.set_num = sub.set_num AND ci.version = 1 WHERE i.version = 1) AS arms (k1, c1, c2) \
     JOIN lego_inventory_parts ip ON ip.inventory_id = arms.c1 JOIN lego_colors col ON col.id = ip.color_id \
     WHERE arms.k1 IN (SELECT k1 FROM side_a) \
     GROUP BY arms.k1, col.name) AS side_b (k1, g2, p2) \
     ON side_a.k1 = side_b.k1 GROUP BY side_a.g1, side_b.g2 ORDER BY bricks DESC, 1, 2 LIMIT 10";

#[test]
fn a_with_query_read_once_is_read_as_its_relations_to_find_the_cut() {
    assert_eq!(on_lego(OWNED), format!("{OWNED_FOLDED}\n"));
}

#[test]
fn the_cut_is_found_whatever_keys_the_tables_have() {
    assert_eq!(
        respell(&lego_without_keys(), OWNED).unwrap(),
        format!("{OWNED_FOLDED}\n")
    );
    assert_eq!(
        respell(&lego_without_keys(), ZONES).unwrap(),
        on_lego(ZONES)
    );
}

#[test]
fn a_respelled_statement_respelled_again_is_left_alone() {
    as_read_on_lego(OWNED_FOLDED);
    let zones = on_lego(ZONES);
    as_read_on_lego(zones.trim_end());
}

#[test]
fn a_parameter_outside_the_grouped_select_stays_where_it_was() {
    let sql = OWNED.replace("c.builder_id < 50", "c.builder_id < ?");
    assert_eq!(
        on_lego(&sql),
        format!(
            "{}\n",
            OWNED_FOLDED.replace("c.builder_id < 50", "c.builder_id < ?")
        )
    );
}

#[test]
fn a_derived_table_is_kept_whole_when_the_cut_needs_no_more() {
    assert_eq!(
        on_lego(
            "SELECT z.home_zone, COUNT(*) AS n FROM (SELECT b.home_zone, b.builder_id FROM lego_builders b \
             WHERE b.builder_id < 100) z JOIN lego_collection c ON c.builder_id = z.builder_id \
             GROUP BY z.home_zone ORDER BY n DESC, z.home_zone LIMIT 3"
        ),
        "WITH side_a (k1, g1, n) AS (SELECT z.builder_id, z.home_zone, COUNT(*) \
         FROM (SELECT b.home_zone, b.builder_id FROM lego_builders b WHERE b.builder_id < 100) z \
         GROUP BY z.builder_id, z.home_zone) \
         SELECT side_a.g1 AS home_zone, CAST(SUM(side_a.n * side_b.n) AS SIGNED) AS n \
         FROM side_a \
         JOIN (SELECT c.builder_id, COUNT(*) FROM lego_collection c WHERE c.builder_id IN (SELECT k1 FROM side_a) \
         GROUP BY c.builder_id) AS side_b (k1, n) \
         ON side_a.k1 = side_b.k1 GROUP BY side_a.g1 ORDER BY n DESC, side_a.g1 LIMIT 3\n"
    );
}

#[test]
fn columns_joined_across_the_cut_join_the_sides_on_each() {
    assert_eq!(
        on_lego(
            "SELECT b.home_zone, COUNT(DISTINCT p.purchase_id) AS n, MAX(CONCAT(p.store, b.name)) AS m, SUM(c.row_no) AS r \
             FROM lego_purchases p JOIN lego_builders b ON b.builder_id = p.builder_id \
             JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no GROUP BY b.home_zone"
        ),
        "WITH side_a (k1, k2, g1, n, p2) AS (SELECT p.builder_id, p.row_no, b.home_zone, COUNT(*), \
         MAX(CONCAT(p.store, b.name)) FROM lego_purchases p JOIN lego_builders b ON b.builder_id = p.builder_id \
         GROUP BY p.builder_id, p.row_no, b.home_zone) \
         SELECT side_a.g1 AS home_zone, CAST(SUM(side_a.n) AS SIGNED) AS n, MAX(side_a.p2) AS m, \
         SUM(side_b.p3 * side_a.n) AS r \
         FROM side_a \
         JOIN (SELECT c.builder_id, c.row_no, SUM(c.row_no) FROM lego_collection c \
         WHERE (c.builder_id, c.row_no) IN (SELECT k1, k2 FROM side_a) GROUP BY c.builder_id, c.row_no) \
         AS side_b (k1, k2, p3) ON side_a.k1 = side_b.k1 AND side_a.k2 = side_b.k2 GROUP BY side_a.g1\n"
    );
}

#[test]
fn a_group_reading_both_sides_is_left_alone() {
    as_read_on_lego(
        "SELECT CONCAT(b.home_zone, c.set_num) AS z, COUNT(*) AS n \
         FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id GROUP BY 1",
    );
}

/// The builders joined to their collection rows, by home zone, with one aggregate.
fn by_zone(aggregate: &str) -> String {
    format!(
        "SELECT b.home_zone, {aggregate} AS a \
         FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id GROUP BY b.home_zone"
    )
}

#[test]
fn an_aggregate_reading_both_sides_is_left_alone() {
    as_read_on_lego(&by_zone("SUM(c.row_no * b.builder_id)"));
}

#[test]
fn an_average_is_left_alone() {
    as_read_on_lego(&by_zone("AVG(c.row_no)"));
}

#[test]
fn a_sum_of_distinct_values_is_left_alone() {
    as_read_on_lego(&by_zone("SUM(DISTINCT c.row_no)"));
}

#[test]
fn a_group_concat_is_left_alone() {
    as_read_on_lego(&by_zone("GROUP_CONCAT(c.set_num)"));
}

#[test]
fn a_sum_that_is_not_of_whole_numbers_or_decimals_is_left_alone() {
    as_read_on_lego(&by_zone("SUM(c.row_no / 2)"));
}

#[test]
fn a_count_of_distinct_values_that_are_not_a_key_is_left_alone() {
    as_read_on_lego(&by_zone("COUNT(DISTINCT c.set_num)"));
}

#[test]
fn a_count_of_distinct_values_of_both_sides_is_left_alone() {
    as_read_on_lego(&by_zone("COUNT(DISTINCT b.builder_id, c.typed_name)"));
}

#[test]
fn a_count_of_distinct_values_that_may_be_null_is_left_alone() {
    as_read_on_lego(&by_zone(
        "COUNT(DISTINCT c.builder_id, c.row_no, c.typed_name)",
    ));
}

/// The purchases by the zone of the builder a join reaches, and their collection rows.
const COUNTED: &str = "SELECT b.home_zone, COUNT(DISTINCT p.purchase_id) AS n, MAX(CONCAT(p.store, b.name)) AS m, \
     SUM(c.row_no) AS r FROM lego_purchases p JOIN lego_builders b ON b.home_zone = p.store \
     JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no GROUP BY b.home_zone";

#[test]
fn a_counted_side_joined_on_a_column_that_is_not_unique_is_left_alone() {
    as_read_on_lego(COUNTED);
}

#[test]
fn a_counted_side_joined_to_a_table_the_catalog_gives_no_key_is_left_alone() {
    let keyed = COUNTED.replace("b.home_zone = p.store", "b.builder_id = p.builder_id");
    assert_ne!(on_lego(&keyed), keyed);
    let unkeyed: Vec<&str> = LEGO
        .lines()
        .filter(|l| !l.starts_with("key\tlego\tlego_builders\t"))
        .collect();
    let c = Catalog::parse(&unkeyed.join("\n")).unwrap();
    assert_eq!(respell(&c, &keyed).unwrap(), keyed);
}

#[test]
fn an_outer_join_is_left_alone() {
    as_read_on_lego(
        "SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b LEFT JOIN lego_collection c ON c.builder_id = b.builder_id \
         GROUP BY b.home_zone",
    );
}

#[test]
fn sides_joined_by_an_inequality_are_left_alone() {
    as_read_on_lego(
        "SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id > b.builder_id \
         GROUP BY b.home_zone",
    );
}

#[test]
fn sides_joined_under_an_or_are_left_alone() {
    as_read_on_lego(
        "SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id \
         OR c.row_no = 1 GROUP BY b.home_zone",
    );
}

#[test]
fn a_lateral_derived_table_reading_the_select_around_it_is_left_alone() {
    as_read_on_lego(
        "SELECT x.home_zone, x.n FROM lego_builders o CROSS JOIN LATERAL (SELECT b.home_zone, COUNT(*) AS n \
         FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id WHERE b.builder_id = o.builder_id \
         GROUP BY b.home_zone) AS x",
    );
}

#[test]
fn a_subquery_reading_the_select_around_it_is_left_alone() {
    as_read_on_lego(
        "SELECT o.builder_id FROM lego_builders o WHERE EXISTS (SELECT b.home_zone, COUNT(*) AS n \
         FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id WHERE b.builder_id = o.builder_id \
         GROUP BY b.home_zone)",
    );
}

#[test]
fn a_column_no_relation_holds_is_left_alone() {
    as_read_on_lego(
        "SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id \
         WHERE c.colour = 1 GROUP BY b.home_zone",
    );
}

#[test]
fn a_table_the_catalog_does_not_hold_is_left_alone() {
    as_read_on_lego(
        "SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN shelf c ON c.builder_id = b.builder_id \
         GROUP BY b.home_zone",
    );
}

/// The builders joined to their collection rows under a condition, by home zone.
fn counted_where(condition: &str) -> String {
    format!(
        "SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id \
         WHERE {condition} GROUP BY b.home_zone"
    )
}

#[test]
fn a_grouped_select_holding_a_parameter_is_left_alone() {
    as_read_on_lego(&counted_where("c.row_no = ?"));
}

#[test]
fn a_grouped_select_holding_a_changing_function_is_left_alone() {
    as_read_on_lego(&counted_where("c.row_no = RAND()"));
}

#[test]
fn a_grouped_select_holding_a_user_variable_is_left_alone() {
    as_read_on_lego(&counted_where("c.row_no = @r"));
}

#[test]
fn a_statement_holding_two_bars_is_left_alone() {
    // MySQL reads `||` as OR unless the session's sql_mode has PIPES_AS_CONCAT
    let sql = counted_where("c.row_no = 1 || c.row_no = 2 AND b.name = 'x'");
    as_read_on_lego(&sql);
    let read = sql::parse_query(&sql).unwrap();
    let mut q = read.clone();
    respell_query(&lego(), &mut q);
    assert_eq!(q, read, "the query was respelled");
}

#[test]
fn a_statement_holding_two_ampersands_is_left_alone() {
    // MySQL reads `&&` as AND, binding as loosely as AND
    as_read_on_lego(&counted_where("c.row_no = 1 && c.set_num = 'x'"));
}

#[test]
fn two_bars_or_ampersands_in_a_string_or_a_comment_do_not_stop_the_respelling() {
    let sql = counted_where("c.set_num = 'a||b&&c' /* || && */");
    assert_ne!(on_lego(&sql), sql, "the statement was not respelled");
}

#[test]
fn a_grouped_select_with_having_is_left_alone() {
    as_read_on_lego(
        "SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id \
         GROUP BY b.home_zone HAVING COUNT(*) > 1",
    );
}

#[test]
fn a_distinct_grouped_select_is_left_alone() {
    as_read_on_lego(
        "SELECT DISTINCT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id \
         GROUP BY b.home_zone",
    );
}

#[test]
fn a_grouped_select_with_rollup_is_left_alone() {
    as_read_on_lego(
        "SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id \
         GROUP BY b.home_zone WITH ROLLUP",
    );
}

#[test]
fn a_select_with_no_group_by_is_left_alone() {
    as_read_on_lego("SELECT COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id");
}

#[test]
fn a_grouped_select_returning_an_unnamed_computed_column_is_left_alone() {
    as_read_on_lego(
        "SELECT b.home_zone, COUNT(*) FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id \
         GROUP BY b.home_zone",
    );
}

#[test]
fn a_grouped_select_ordered_by_a_column_it_does_not_group_by_is_left_alone() {
    as_read_on_lego(
        "SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id \
         GROUP BY b.home_zone ORDER BY c.set_num",
    );
}

#[test]
fn the_relation_every_arm_joins_may_be_joined_on_another_column_of_the_rest_in_each() {
    assert_eq!(
        on_lego(
            "SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
             UNION ALL SELECT c.builder_id, i.id FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.typed_set_num"
        ),
        "SELECT arms.c1 AS builder_id, i.id FROM lego_inventories i JOIN (SELECT c.set_num, c.builder_id \
         FROM lego_collection c UNION ALL SELECT c.typed_set_num, c.builder_id FROM lego_collection c) AS arms (k1, c1) \
         ON i.set_num = arms.k1\n"
    );
}

#[test]
fn arms_whose_rest_is_not_joined_within_itself_are_left_alone() {
    as_read_on_lego(
        "SELECT c.builder_id, i.id, s.year FROM lego_collection c JOIN lego_inventories i ON i.set_num = c.set_num \
         JOIN lego_sets s ON s.set_num = c.set_num \
         UNION ALL SELECT c.builder_id, sub.quantity, t.id FROM lego_collection c \
         JOIN lego_inventory_sets sub ON sub.set_num = c.set_num JOIN lego_themes t ON t.name = c.set_num",
    );
}

#[test]
fn a_with_query_whose_table_a_later_with_query_names_is_not_read_as_its_relations() {
    let sql = OWNED.replace(
        " SELECT b.home_zone,",
        ", lego_inventory_sets AS (SELECT 1 AS inventory_id) SELECT b.home_zone,",
    );
    let out = on_lego(&sql);
    assert!(!out.contains("side_a"), "{out}");
    assert!(out.contains("AS arms (k1, c1, c2)"), "{out}");
}

#[test]
fn each_side_is_joined_within_itself() {
    // the themes alone on one side: the builders and the collection rows are joined to each other
    assert_eq!(
        on_lego(
            "SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id \
             JOIN lego_themes t ON t.name = b.name GROUP BY b.home_zone"
        ),
        "WITH side_a (k1, g1, n) AS (SELECT b.name, b.home_zone, COUNT(*) FROM lego_builders b \
         JOIN lego_collection c ON c.builder_id = b.builder_id GROUP BY b.name, b.home_zone) \
         SELECT side_a.g1 AS home_zone, CAST(SUM(side_a.n * side_b.n) AS SIGNED) AS n \
         FROM side_a \
         JOIN (SELECT t.name, COUNT(*) FROM lego_themes t WHERE t.name IN (SELECT k1 FROM side_a) GROUP BY t.name) \
         AS side_b (k1, n) ON side_a.k1 = side_b.k1 GROUP BY side_a.g1\n"
    );
}

#[test]
fn columns_of_other_types_joined_across_the_cut_are_left_alone() {
    as_read_on_lego(
        "SELECT b.home_zone, COUNT(*) AS n FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id \
         JOIN lego_themes t ON t.name = b.builder_id GROUP BY b.home_zone",
    );
}

#[test]
fn a_side_whose_rows_are_not_counted_gives_a_row_per_group() {
    assert_eq!(
        on_lego(
            "SELECT b.home_zone, COUNT(DISTINCT c.builder_id, c.row_no) AS n \
             FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id GROUP BY b.home_zone"
        ),
        "WITH side_b (k1, n) AS (SELECT c.builder_id, COUNT(*) FROM lego_collection c GROUP BY c.builder_id) \
         SELECT side_a.g1 AS home_zone, CAST(SUM(side_b.n) AS SIGNED) AS n \
         FROM (SELECT b.builder_id, b.home_zone FROM lego_builders b WHERE b.builder_id IN (SELECT k1 FROM side_b) \
         GROUP BY b.builder_id, b.home_zone) AS side_a (k1, g1) \
         JOIN side_b ON side_a.k1 = side_b.k1 GROUP BY side_a.g1\n"
    );
}

#[test]
fn a_with_query_read_twice_is_not_read_as_its_relations() {
    let twice = OWNED.replace(
        " SELECT b.home_zone,",
        ", twice AS (SELECT builder_id FROM held) SELECT b.home_zone,",
    );
    let out = on_lego(&twice);
    assert!(
        !out.contains("side_a") && out.contains("AS arms (k1, c1, c2)"),
        "{out}"
    );
}

#[test]
fn a_with_query_holding_a_name_the_select_gives_another_relation_is_not_read_as_its_relations() {
    let alike = OWNED
        .replace(
            "lego_builders b ON b.builder_id",
            "lego_builders o ON o.builder_id",
        )
        .replace("SELECT b.home_zone", "SELECT o.home_zone");
    let out = on_lego(&alike);
    assert!(
        !out.contains("side_a") && out.contains("AS arms (k1, c1, c2)"),
        "{out}"
    );
}

#[test]
fn a_count_of_a_column_is_its_sides_count_times_the_other_sides_rows() {
    assert_eq!(
        on_lego(
            "SELECT b.home_zone, COUNT(c.typed_name) AS t \
             FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id GROUP BY b.home_zone"
        ),
        "WITH side_a (k1, g1, n) AS (SELECT b.builder_id, b.home_zone, COUNT(*) FROM lego_builders b \
         GROUP BY b.builder_id, b.home_zone) \
         SELECT side_a.g1 AS home_zone, CAST(SUM(side_b.p1 * side_a.n) AS SIGNED) AS t \
         FROM side_a \
         JOIN (SELECT c.builder_id, COUNT(*), COUNT(c.typed_name) FROM lego_collection c \
         WHERE c.builder_id IN (SELECT k1 FROM side_a) GROUP BY c.builder_id) \
         AS side_b (k1, n, p1) ON side_a.k1 = side_b.k1 GROUP BY side_a.g1\n"
    );
}

#[test]
fn the_cut_with_the_fewest_relations_on_its_smaller_side_is_taken_the_first_in_order() {
    // the collection rows alone, or the themes alone, on one side: both cuts hold
    assert_eq!(
        on_lego(
            "SELECT b.home_zone, MAX(c.set_num) AS m, MIN(t.name) AS t FROM lego_builders b \
             JOIN lego_collection c ON c.builder_id = b.builder_id JOIN lego_themes t ON t.name = b.name \
             GROUP BY b.home_zone"
        ),
        "WITH side_a (k1, g1, p2) AS (SELECT b.builder_id, b.home_zone, MIN(t.name) FROM lego_builders b \
         JOIN lego_themes t ON t.name = b.name GROUP BY b.builder_id, b.home_zone) \
         SELECT side_a.g1 AS home_zone, MAX(side_b.p1) AS m, MIN(side_a.p2) AS t \
         FROM side_a \
         JOIN (SELECT c.builder_id, MAX(c.set_num) FROM lego_collection c WHERE c.builder_id IN (SELECT k1 FROM side_a) \
         GROUP BY c.builder_id) AS side_b (k1, p1) \
         ON side_a.k1 = side_b.k1 GROUP BY side_a.g1\n"
    );
}

#[test]
fn the_side_whose_rows_a_sum_counts_holds_the_keys_the_other_is_grouped_over() {
    assert_eq!(
        on_lego(
            "SELECT c.set_num, SUM(b.builder_id) AS s \
             FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id GROUP BY c.set_num"
        ),
        "WITH side_b (k1, g1, n) AS (SELECT c.builder_id, c.set_num, COUNT(*) FROM lego_collection c \
         GROUP BY c.builder_id, c.set_num) \
         SELECT side_b.g1 AS set_num, SUM(side_a.p1 * side_b.n) AS s \
         FROM (SELECT b.builder_id, SUM(b.builder_id) FROM lego_builders b WHERE b.builder_id IN (SELECT k1 FROM side_b) \
         GROUP BY b.builder_id) AS side_a (k1, p1) \
         JOIN side_b ON side_a.k1 = side_b.k1 GROUP BY side_b.g1\n"
    );
}

#[test]
fn the_side_a_count_of_distinct_values_reads_holds_the_keys_the_other_is_grouped_over() {
    // both sides' rows are counted
    assert_eq!(
        on_lego(
            "SELECT b.home_zone, COUNT(DISTINCT c.builder_id, c.row_no) AS n, COUNT(*) AS r \
             FROM lego_builders b JOIN lego_collection c ON c.builder_id = b.builder_id GROUP BY b.home_zone"
        ),
        "WITH side_b (k1, n) AS (SELECT c.builder_id, COUNT(*) FROM lego_collection c GROUP BY c.builder_id) \
         SELECT side_a.g1 AS home_zone, CAST(SUM(side_b.n) AS SIGNED) AS n, CAST(SUM(side_a.n * side_b.n) AS SIGNED) AS r \
         FROM (SELECT b.builder_id, b.home_zone, COUNT(*) FROM lego_builders b WHERE b.builder_id IN (SELECT k1 FROM side_b) \
         GROUP BY b.builder_id, b.home_zone) AS side_a (k1, g1, n) \
         JOIN side_b ON side_a.k1 = side_b.k1 GROUP BY side_a.g1\n"
    );
}
