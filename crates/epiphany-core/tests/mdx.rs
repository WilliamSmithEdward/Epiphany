use epiphany_core::Scalar;
use epiphany_core::cells::CellError;
use epiphany_core::database::{CellWrite, Database, Error};
use epiphany_core::mdx::{Asker, MemberType, QueryResult};
use epiphany_core::model::ModelDef;

const CUBE: &str = "General Ledger";

fn set(at: [&str; 4], value: f64) -> CellWrite {
    CellWrite {
        cube: CUBE.to_owned(),
        at: at.map(str::to_owned).to_vec(),
        value: Scalar::Number(value),
    }
}

/// The example model with a few actuals and one budget figure.
fn database() -> Database {
    let dimensions = [
        include_str!("fixtures/model/dimensions/account.json"),
        include_str!("fixtures/model/dimensions/entity.json"),
        include_str!("fixtures/model/dimensions/period.json"),
        include_str!("fixtures/model/dimensions/version.json"),
    ];
    let cube = include_str!("fixtures/model/cubes/general-ledger.json");
    let definition = ModelDef {
        dimensions: dimensions
            .iter()
            .map(|text| serde_json::from_str(text).unwrap())
            .collect(),
        cubes: vec![serde_json::from_str(cube).unwrap()],
        ..ModelDef::default()
    };
    let mut database = Database::new(definition).unwrap();
    let writes = [
        set(["Product Revenue", "US", "Jan", "Actual"], 100.0),
        set(["Product Revenue", "UK", "Jan", "Actual"], 50.0),
        set(["Product Revenue", "US", "Feb", "Actual"], 20.0),
        set(["Service Revenue", "US", "Jan", "Actual"], 30.0),
        set(["Rent", "US", "Jan", "Actual"], 10.0),
        set(["Salaries", "UK", "Feb", "Actual"], 40.0),
        set(["Product Revenue", "US", "Jan", "Budget"], 90.0),
    ];
    database.write(None, &writes).unwrap();
    database
}

fn run(mdx: &str) -> QueryResult {
    database()
        .query(None, mdx)
        .unwrap_or_else(|error| panic!("{error}\n{mdx}"))
}

fn failure(mdx: &str) -> String {
    match database().query(None, mdx) {
        Err(Error::Mdx(error)) => error.to_string(),
        other => panic!("expected an MDX error, found {other:?}"),
    }
}

/// The tuples of an axis, each as its member names joined by a slash.
fn names(result: &QueryResult, axis: usize) -> Vec<String> {
    let name = |tuple: &Vec<epiphany_core::mdx::MemberResult>| {
        let names: Vec<&str> = tuple.iter().map(|member| member.name.as_str()).collect();
        names.join(" / ")
    };
    result.axes[axis].tuples.iter().map(name).collect()
}

fn numbers(result: &QueryResult) -> Vec<Option<f64>> {
    let number = |cell: &Option<Scalar>| match cell {
        Some(Scalar::Number(number)) => Some(*number),
        Some(Scalar::Text(text)) => panic!("expected a number, found '{text}'"),
        None => None,
    };
    result.cells.iter().map(number).collect()
}

fn some(numbers: &[f64]) -> Vec<Option<f64>> {
    numbers.iter().copied().map(Some).collect()
}

/// The column names of a query that puts one set on columns over actuals.
fn columns(set: &str) -> Vec<String> {
    let mdx = format!("SELECT {set} ON COLUMNS FROM [General Ledger] WHERE [Version].[Actual]");
    names(&run(&mdx), 0)
}

#[test]
fn columns_rows_and_a_slicer_address_cells() {
    let result = run("
        SELECT {[Entity].[US], [Entity].[UK], [Entity].[Total Entity]} ON COLUMNS,
               {[Account].[Product Revenue], [Account].[Revenue]} ON ROWS
        FROM [General Ledger]
        WHERE ([Period].[Jan], [Version].[Actual])");
    assert_eq!(result.cube, "General Ledger");
    let slicer: Vec<&str> = result
        .slicer
        .iter()
        .map(|m| m.unique_name.as_str())
        .collect();
    assert_eq!(slicer, ["[Period].[Jan]", "[Version].[Actual]"]);
    assert_eq!(names(&result, 0), ["US", "UK", "Total Entity"]);
    assert_eq!(names(&result, 1), ["Product Revenue", "Revenue"]);
    assert_eq!(
        numbers(&result),
        some(&[100.0, 50.0, 150.0, 130.0, 50.0, 180.0])
    );
}

#[test]
fn members_stood_at_take_the_slicers_place() {
    let database = database();
    let stood = |mdx: &str, standing: &[(&str, &str)]| {
        database.query_as(
            None,
            mdx,
            &Asker {
                standing,
                ..Asker::ANYONE
            },
        )
    };
    let unique = |result: &QueryResult| -> Vec<String> {
        result
            .slicer
            .iter()
            .map(|m| m.unique_name.clone())
            .collect()
    };
    let budget = stood(
        "SELECT {[Entity].[US]} ON COLUMNS, {[Account].[Product Revenue]} ON ROWS
         FROM [General Ledger] WHERE ([Period].[Jan], [Version].[Actual])",
        &[("Version", "Budget")],
    )
    .unwrap();
    assert_eq!(unique(&budget), ["[Period].[Jan]", "[Version].[Budget]"]);
    assert_eq!(numbers(&budget), some(&[90.0]));

    // A dimension the WHERE leaves out is added, found by any spelling of its name; one an
    // axis places keeps only the axis's members at it or beneath it, and is not reported
    // among the slicer.
    let added = stood(
        "SELECT {[Entity].[US], [Entity].[UK]} ON COLUMNS FROM [General Ledger]
         WHERE [Version].[Actual]",
        &[
            ("Period", "Jan"),
            ("Entity", "UK"),
            ("account", "product revenue"),
        ],
    )
    .unwrap();
    assert_eq!(
        unique(&added),
        [
            "[Version].[Actual]",
            "[Period].[Jan]",
            "[Account].[Product Revenue]"
        ]
    );
    assert_eq!(names(&added, 0), ["UK"]);
    assert_eq!(numbers(&added), some(&[50.0]));

    // Stood at a total, an axis keeps the total and every member beneath it, and drops a member
    // above it; a query's own calculated member stays.
    let beneath = stood(
        "WITH MEMBER [Entity].[Twice US] AS [Entity].[US] * 2
         SELECT {[Entity].[Total Entity], [Entity].[US], [Entity].[Twice US]} ON COLUMNS,
                {[Period].[H1], [Period].[Q1], [Period].[Jan], [Period].[Apr]} ON ROWS
         FROM [General Ledger] WHERE ([Account].[Product Revenue], [Version].[Actual])",
        &[("Period", "Q1"), ("Entity", "US")],
    )
    .unwrap();
    assert_eq!(names(&beneath, 0), ["US", "Twice US"]);
    assert_eq!(names(&beneath, 1), ["Q1", "Jan"]);

    let unknown = stood(
        "SELECT {[Entity].[US]} ON COLUMNS FROM [General Ledger]",
        &[("Version", "Forecast")],
    );
    assert!(matches!(unknown, Err(Error::Mdx(_))), "{unknown:?}");
}

#[test]
fn rows_may_come_before_columns_and_axes_may_be_numbered() {
    let result = run("
        SELECT {[Account].[Revenue]} ON 1, {[Entity].[US], [Entity].[UK]} ON AXIS(0)
        FROM [General Ledger] WHERE ([Period].[Jan], [Version].[Actual])");
    assert_eq!(names(&result, 0), ["US", "UK"]);
    assert_eq!(numbers(&result), some(&[130.0, 50.0]));
}

#[test]
fn dimensions_not_mentioned_stand_at_their_top_element() {
    // Net Income, Total Entity, H1 and Variance: actuals of 150 less a budget of 90.
    let result = run("SELECT FROM [General Ledger]");
    assert!(result.axes.is_empty());
    assert_eq!(numbers(&result), some(&[60.0]));
}

#[test]
fn names_ignore_case_and_accept_aliases_and_bare_words() {
    let result = run("
        select {[entity].[us]} on columns
        from [general ledger] where ([PERIOD].[m02], Version.Actual, [Account].[Account].[Revenue])");
    assert_eq!(numbers(&result), some(&[20.0]));
}

#[test]
fn members_and_children_list_elements_in_declared_order() {
    assert_eq!(columns("[Entity].Members"), ["Total Entity", "US", "UK"]);
    assert_eq!(columns("[Period].[Q1].Children"), ["Jan", "Feb", "Mar"]);
    assert_eq!(
        columns("{[Period].[Jan].Parent, [Period].[Q2].FirstChild, [Period].[Q2].LastChild}"),
        ["Q1", "Apr", "Jun"]
    );
}

#[test]
fn sets_cross_join_with_a_function_or_a_star() {
    let expected = ["US / Jan", "US / Feb", "UK / Jan", "UK / Feb"];
    for set in [
        "CrossJoin({[Entity].[US], [Entity].[UK]}, {[Period].[Jan], [Period].[Feb]})",
        "{[Entity].[US], [Entity].[UK]} * {[Period].[Jan], [Period].[Feb]}",
    ] {
        let result = run(&format!(
            "SELECT {set} ON COLUMNS FROM [General Ledger]
             WHERE ([Account].[Product Revenue], [Version].[Actual])"
        ));
        assert_eq!(names(&result, 0), expected);
        assert_eq!(numbers(&result), some(&[100.0, 20.0, 50.0, 0.0]));
    }
}

#[test]
fn filter_keeps_the_tuples_that_meet_a_condition() {
    assert_eq!(
        columns("Filter([Period].[Q1].Children, [Account].[Revenue] > 100)"),
        ["Jan"]
    );
    assert_eq!(
        columns(
            "Filter([Period].[Q1].Children,
                 NOT IsEmpty([Account].[Salaries]) OR [Period].CurrentMember.Name = \"Mar\")"
        ),
        ["Feb", "Mar"]
    );
    assert_eq!(
        columns(
            "Filter([Entity].Members, [Entity].CurrentMember.Name <> 'US'
                 AND ([Account].[Revenue], [Period].[Jan]) >= 50)"
        ),
        ["Total Entity", "UK"]
    );
}

#[test]
fn order_sorts_by_value_alone_or_within_the_hierarchy() {
    let quarter = "[Period].[Q1].Children, [Account].[Revenue]";
    assert_eq!(
        columns(&format!("Order({quarter}, BASC)")),
        ["Mar", "Feb", "Jan"]
    );
    assert_eq!(
        columns(&format!("Order({quarter}, BDESC)")),
        ["Jan", "Feb", "Mar"]
    );

    // January: Net Income 170 = Revenue 180 (150 + 30) less Operating Expenses 10 (0 + 10).
    assert_eq!(
        columns("Order([Account].Members, [Period].[Jan], DESC)"),
        [
            "Net Income",
            "Revenue",
            "Product Revenue",
            "Service Revenue",
            "Operating Expenses",
            "Rent",
            "Salaries"
        ]
    );
    assert_eq!(
        columns("Order([Account].Members, [Period].[Jan])"),
        [
            "Net Income",
            "Operating Expenses",
            "Salaries",
            "Rent",
            "Revenue",
            "Service Revenue",
            "Product Revenue"
        ]
    );
    assert_eq!(
        columns("Order([Account].Members, [Period].[Jan], BDESC)")[..3],
        ["Revenue", "Net Income", "Product Revenue"]
    );
}

#[test]
fn descendants_walk_down_the_hierarchy() {
    let half = ["H1", "Q1", "Jan", "Feb", "Mar", "Q2", "Apr", "May", "Jun"];
    assert_eq!(columns("Descendants([Period].[H1])"), half);
    assert_eq!(columns("Descendants([Period].[H1], 0)"), ["H1"]);
    assert_eq!(columns("Descendants([Period].[H1], 1)"), ["Q1", "Q2"]);
    assert_eq!(
        columns("Descendants([Period].[H1], 1, SELF_AND_AFTER)"),
        half[1..]
    );
    assert_eq!(
        columns("Descendants([Period].[H1], 1, AFTER)"),
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun"]
    );
    assert_eq!(
        columns("Descendants([Period].[Q2], 0, LEAVES)"),
        ["Apr", "May", "Jun"]
    );
    // Before the distance, from the member itself down; and with it, or with what comes after.
    assert_eq!(columns("Descendants([Period].[H1], 1, BEFORE)"), ["H1"]);
    assert_eq!(
        columns("Descendants([Period].[H1], 1, SELF_AND_BEFORE)"),
        ["H1", "Q1", "Q2"]
    );
    assert_eq!(
        columns("Descendants([Period].[H1], 1, BEFORE_AND_AFTER)"),
        ["H1", "Jan", "Feb", "Mar", "Apr", "May", "Jun"]
    );
    assert_eq!(
        columns("Descendants([Period].[H1], 1, SELF_BEFORE_AFTER)"),
        half
    );
}

#[test]
fn ancestors_walk_up_the_hierarchy() {
    assert_eq!(columns("Ancestors([Period].[Jan], 2)"), ["H1"]);
    assert_eq!(
        columns("{Ancestor([Period].[Jan], 1), Ancestor([Period].[Jan], 0)}"),
        ["Q1", "Jan"]
    );
}

#[test]
fn hierarchize_puts_members_in_hierarchy_order() {
    let scrambled = "{[Period].[Mar], [Period].[Q1], [Period].[H1], [Period].[Jan]}";
    assert_eq!(
        columns(&format!("Hierarchize({scrambled})")),
        ["H1", "Q1", "Jan", "Mar"]
    );
    assert_eq!(
        columns(&format!("Hierarchize({scrambled}, POST)")),
        ["Jan", "Mar", "Q1", "H1"]
    );
}

#[test]
fn non_empty_drops_empty_tuples() {
    let result = run("
        SELECT {[Period].[Jan], [Period].[Feb]} ON COLUMNS,
               NON EMPTY [Account].Members ON ROWS
        FROM [General Ledger] WHERE ([Entity].[UK], [Version].[Actual])");
    assert_eq!(
        names(&result, 1),
        [
            "Net Income",
            "Revenue",
            "Product Revenue",
            "Operating Expenses",
            "Salaries"
        ]
    );
    assert_eq!(numbers(&result)[..4], some(&[50.0, -40.0, 50.0, 0.0]));

    let result = run("
        SELECT NON EMPTY {[Period].[Jan], [Period].[Feb], [Period].[Mar]} ON COLUMNS,
               NON EMPTY {[Account].[Rent], [Account].[Salaries]} ON ROWS
        FROM [General Ledger] WHERE ([Entity].[UK], [Version].[Actual])");
    assert_eq!(
        (names(&result, 0), names(&result, 1)),
        (vec!["Feb".to_owned()], vec!["Salaries".to_owned()])
    );
    assert_eq!(numbers(&result), some(&[40.0]));

    let mdx = "
        SELECT NonEmpty([Period].[Q1].Children, {[Account].[Rent]}) ON COLUMNS
        FROM [General Ledger] WHERE ([Entity].[US], [Version].[Actual])";
    assert_eq!(names(&run(mdx), 0), ["Jan"]);
    assert_eq!(
        columns("NonEmpty([Period].[Q2].Children)"),
        Vec::<String>::new()
    );
}

#[test]
fn calculated_members_are_evaluated_cell_by_cell() {
    let result = run("
        WITH MEMBER [Account].[Margin] AS [Account].[Revenue] - [Account].[Operating Expenses]
             MEMBER [Account].[Margin %] AS
                 IIF([Account].[Revenue] = 0, 0, [Account].[Margin] / [Account].[Revenue])
             MEMBER [Account].[Per Rent] AS '[Account].[Revenue] / [Account].[Rent]'
        SELECT {[Entity].[US], [Entity].[UK]} ON COLUMNS,
               {[Account].[Margin], [Account].[Margin %], [Account].[Per Rent]} ON ROWS
        FROM [General Ledger] WHERE ([Period].[Jan], [Version].[Actual])");
    assert_eq!(names(&result, 1), ["Margin", "Margin %", "Per Rent"]);
    // The UK has no rent in January, and dividing by nothing gives no value.
    assert_eq!(
        numbers(&result),
        [
            Some(120.0),
            Some(50.0),
            Some(120.0 / 130.0),
            Some(1.0),
            Some(13.0),
            None
        ]
    );
    let margin = &result.axes[1].tuples[0][0];
    assert_eq!((margin.kind, margin.level), (MemberType::Calculated, 0));
    assert_eq!(margin.unique_name, "[Account].[Margin]");
}

#[test]
fn a_calculated_member_may_sit_in_the_slicer_or_another_dimension() {
    let result = run("
        WITH MEMBER [Version].[Shortfall] AS [Version].[Budget] - [Version].[Actual]
        SELECT {[Account].[Product Revenue], [Account].[Revenue]} ON COLUMNS
        FROM [General Ledger] WHERE ([Entity].[US], [Period].[Jan], [Version].[Shortfall])");
    assert_eq!(numbers(&result), some(&[-10.0, -40.0]));
}

#[test]
fn named_sets_are_reused_by_name() {
    let result = run("
        WITH SET [Quarter] AS [Period].[Q1].Children
             SET Busy AS Filter([Quarter], [Account].[Revenue] > 0)
             SET Pairs AS {[Entity].[US]} * [Quarter]
        SELECT [Busy] ON COLUMNS, {[Entity].[US], [Entity].[UK]} ON ROWS
        FROM [General Ledger] WHERE ([Account].[Revenue], [Version].[Actual])");
    assert_eq!(names(&result, 0), ["Jan", "Feb"]);
    assert_eq!(numbers(&result), some(&[130.0, 20.0, 50.0, 0.0]));

    let result = run("
        WITH SET [Quarter] AS [Period].[Q1].Children
             SET Pairs AS {[Entity].[US]} * [Quarter]
        SELECT Pairs ON COLUMNS FROM [General Ledger]");
    assert_eq!(names(&result, 0), ["US / Jan", "US / Feb", "US / Mar"]);
}

#[test]
fn members_carry_metadata_and_requested_properties() {
    let result = run("
        SELECT [Period].[Q1].Children DIMENSION PROPERTIES [Period].[Days], Code ON COLUMNS,
               {[Account].[Revenue], [Account].[Rent]} PROPERTIES AccountCode ON ROWS
        FROM [General Ledger]");
    let feb = &result.axes[0].tuples[1][0];
    assert_eq!(
        serde_json::to_value(feb).unwrap(),
        serde_json::json!({
            "dimension": "Period",
            "name": "Feb",
            "unique_name": "[Period].[Feb]",
            "type": "numeric",
            "level": 0,
            "properties": { "Code": "M02", "Days": 28.0 }
        })
    );
    let revenue = &result.axes[1].tuples[0][0];
    assert_eq!((revenue.kind, revenue.level), (MemberType::Consolidated, 1));
    assert!(revenue.properties.is_empty());
    let rent = &result.axes[1].tuples[1][0];
    assert_eq!(
        rent.properties["AccountCode"],
        Scalar::Text("6100".to_owned())
    );
}

#[test]
fn expressions_read_member_properties() {
    let result = run("
        WITH MEMBER [Account].[Per Day] AS
                 [Account].[Revenue] / [Period].CurrentMember.Properties(\"Days\")
        SELECT [Period].[Q1].Children ON COLUMNS
        FROM [General Ledger] WHERE ([Account].[Per Day], [Version].[Actual])");
    assert_eq!(numbers(&result), some(&[180.0 / 31.0, 20.0 / 28.0, 0.0]));

    assert_eq!(
        columns("Filter([Period].Members, [Period].CurrentMember.Properties('LEVEL_NUMBER') = 1)"),
        ["Q1", "Q2"]
    );
    assert_eq!(
        columns(
            "Filter([Period].Members,
                 [Period].CurrentMember.Properties('PARENT_UNIQUE_NAME') = '[Period].[Q2]')"
        ),
        ["Apr", "May", "Jun"]
    );
    assert_eq!(
        columns(
            "Filter([Account].Members, [Account].CurrentMember.Properties('AccountType') = 'Expense')"
        ),
        ["Salaries", "Rent"]
    );
}

#[test]
fn a_query_in_a_transaction_sees_its_pending_writes() {
    let mut database = database();
    let transaction = database.begin();
    let write = set(["Rent", "UK", "Jan", "Actual"], 5.0);
    database.write(Some(transaction), &[write]).unwrap();
    let mdx = "SELECT {[Account].[Operating Expenses]} ON COLUMNS FROM [General Ledger]
               WHERE ([Period].[Jan], [Version].[Actual])";
    assert_eq!(
        numbers(&database.query(Some(transaction), mdx).unwrap()),
        some(&[15.0])
    );
    assert_eq!(numbers(&database.query(None, mdx).unwrap()), some(&[10.0]));
}

#[test]
fn aggregates_run_over_a_set() {
    let result = run("
        WITH MEMBER [Account].[Sum] AS Sum([Period].[Q1].Children, [Account].[Revenue])
             MEMBER [Account].[Avg] AS Avg([Period].[Q1].Children, [Account].[Revenue])
             MEMBER [Account].[Min] AS Min([Period].[Q1].Children, [Account].[Revenue])
             MEMBER [Account].[Max] AS Max([Period].[Q1].Children, [Account].[Revenue])
             MEMBER [Account].[Count] AS Count([Period].[Q1].Children)
             MEMBER [Account].[Size] AS [Period].[Q1].Children.Count
             MEMBER [Account].[None] AS Avg([Period].[Q2].Children, [Account].[Revenue])
             MEMBER [Account].[Else] AS CoalesceEmpty([Account].[Salaries], [Account].[Rent], 7)
        SELECT {[Account].[Sum], [Account].[Avg], [Account].[Min], [Account].[Max],
                [Account].[Count], [Account].[Size], [Account].[None], [Account].[Else]} ON COLUMNS
        FROM [General Ledger] WHERE ([Period].[Jan], [Version].[Actual])");
    // Revenue is 180 in January, 20 in February and nothing in March, which the average,
    // least and greatest leave out. January has no salaries but 10 of rent.
    assert_eq!(
        numbers(&result),
        [
            Some(200.0),
            Some(100.0),
            Some(20.0),
            Some(180.0),
            Some(3.0),
            Some(3.0),
            None,
            Some(10.0)
        ]
    );
    assert_eq!(
        columns(
            "Filter([Entity].Members,
                 Count([Period].[Q1].Children * {[Account].[Revenue]}, EXCLUDEEMPTY) > 1)"
        ),
        ["Total Entity", "US"]
    );
}

#[test]
fn sets_are_combined_and_cut() {
    let (first, second) = (
        "{[Period].[Jan], [Period].[Feb]}",
        "{[Period].[Feb], [Period].[Mar]}",
    );
    let quarter = "[Period].[Q1].Children";
    assert_eq!(
        columns(&format!("Union({first}, {second})")),
        ["Jan", "Feb", "Mar"]
    );
    assert_eq!(
        columns(&format!("{first} + {second}")),
        ["Jan", "Feb", "Mar"]
    );
    assert_eq!(
        columns(&format!("Union({first}, {second}, ALL)")),
        ["Jan", "Feb", "Feb", "Mar"]
    );
    assert_eq!(
        columns(&format!("Except({quarter}, {{[Period].[Feb]}})")),
        ["Jan", "Mar"]
    );
    assert_eq!(
        columns(&format!(
            "Intersect({quarter}, {{[Period].[Mar], [Period].[Apr]}})"
        )),
        ["Mar"]
    );
    assert_eq!(
        columns("Distinct({[Period].[Jan], [Period].[Jan], [Period].[Feb]})"),
        ["Jan", "Feb"]
    );
    assert_eq!(columns(&format!("Head({quarter})")), ["Jan"]);
    assert_eq!(columns(&format!("Head({quarter}, 2)")), ["Jan", "Feb"]);
    assert_eq!(columns(&format!("Tail({quarter}, 2)")), ["Feb", "Mar"]);
    assert_eq!(columns(&format!("Subset({quarter}, 1, 1)")), ["Feb"]);
    assert_eq!(columns(&format!("Subset({quarter}, 1)")), ["Feb", "Mar"]);
    assert_eq!(columns(&format!("{{{quarter}.Item(1)}}")), ["Feb"]);
    assert_eq!(
        columns(&format!("TopCount({quarter}, 2, [Account].[Revenue])")),
        ["Jan", "Feb"]
    );
    assert_eq!(
        columns(&format!("BottomCount({quarter}, 1, [Account].[Revenue])")),
        ["Mar"]
    );
    assert_eq!(
        columns("Generate({[Period].[Q1], [Period].[Q2]}, [Period].CurrentMember.Children)"),
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun"]
    );
}

#[test]
fn ranges_siblings_and_all_members() {
    assert_eq!(
        columns("[Period].[Jan]:[Period].[Mar]"),
        ["Jan", "Feb", "Mar"]
    );
    assert_eq!(
        columns("{[Period].[Mar] : [Period].[Feb], [Period].[Jun]}"),
        ["Feb", "Mar", "Jun"]
    );
    assert_eq!(columns("[Period].[Feb].Siblings"), ["Jan", "Feb", "Mar"]);
    assert_eq!(columns("[Period].[H1].Siblings"), ["H1"]);
    assert_eq!(
        columns("Filter([Entity].Members, IsLeaf([Entity].CurrentMember))"),
        ["US", "UK"]
    );
    let result = run("
        WITH MEMBER [Entity].[Both] AS [Entity].[US] + [Entity].[UK]
        SELECT [Entity].AllMembers ON COLUMNS FROM [General Ledger]");
    assert_eq!(names(&result, 0), ["Total Entity", "US", "UK", "Both"]);
}

#[test]
fn tm1_style_set_functions() {
    assert_eq!(
        columns("TM1FilterByLevel(TM1SubsetAll([Period]), 0)"),
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun"]
    );
    assert_eq!(
        columns("TM1FilterByLevel([Period].Members, 1, 2)"),
        ["H1", "Q1", "Q2"]
    );
    assert_eq!(
        columns("TM1FilterByPattern([Period].Members, '*a*')"),
        ["Jan", "Mar", "Apr", "May"]
    );
    assert_eq!(
        columns("TM1FilterByPattern([Period].Members, 'q?')"),
        ["Q1", "Q2"]
    );
    let months = "{[Period].[Mar], [Period].[Jan], [Period].[Feb]}";
    assert_eq!(
        columns(&format!("TM1Sort({months}, ASC)")),
        ["Feb", "Jan", "Mar"]
    );
    assert_eq!(
        columns(&format!("TM1Sort({months}, DESC)")),
        ["Mar", "Jan", "Feb"]
    );
    // By place in the dimension, where the months stand after the quarters and the half.
    let mixed = "{[Period].[Mar], [Period].[Q1], [Period].[Jan], [Period].[H1]}";
    assert_eq!(
        columns(&format!("TM1SortByIndex({mixed}, ASC)")),
        columns(&format!("Intersect([Period].Members, {mixed})"))
    );
    assert_eq!(
        columns(&format!("TM1SortByIndex({months}, DESC)")),
        ["Mar", "Feb", "Jan"]
    );

    let tuple = "([Entity].[UK], [Period].[Mar])";
    assert_eq!(text(&format!("TM1Member({tuple}, 0).Name")), "UK");
    assert_eq!(text(&format!("TM1Member({tuple}, 1).Name")), "Mar");
    assert_eq!(number(&format!("TM1TupleSize({tuple})")), Some(2.0));
    assert_eq!(number("TM1TupleSize([Period].[Mar])"), Some(1.0));
    assert_eq!(
        text(&format!(
            "IIF(TM1Member({tuple}, 2) IS NULL, 'none', 'one')"
        )),
        "none"
    );

    // A member knows its dimension, which is its hierarchy too.
    assert_eq!(text("[Period].[Mar].Dimension.Name"), "Period");
    assert_eq!(text("[Period].[Mar].Hierarchy.UniqueName"), "[Period]");
    assert_eq!(text("[Period].[Mar].Level.Hierarchy.Name"), "Period");
    assert_eq!(
        text("[Period].CurrentMember.Dimension.CurrentMember.Name"),
        "Feb"
    );
    assert_eq!(
        columns("[Period].[Mar].Hierarchy.Members"),
        columns("[Period].Members")
    );
}

#[test]
fn a_query_may_have_more_than_two_axes() {
    let result = run("
        SELECT {[Entity].[US], [Entity].[UK]} ON COLUMNS,
               {[Period].[Jan], [Period].[Feb]} ON ROWS,
               NON EMPTY {[Account].[Product Revenue], [Account].[Salaries], [Account].[Rent]} ON PAGES
        FROM [General Ledger] WHERE [Version].[Actual]");
    assert_eq!(result.axes.len(), 3);
    // Salaries are empty for the US and for January in the UK only; February in the UK has 40.
    assert_eq!(names(&result, 2), ["Product Revenue", "Salaries", "Rent"]);
    assert_eq!(
        numbers(&result),
        some(&[
            100.0, 50.0, 20.0, 0.0, 0.0, 0.0, 0.0, 40.0, 10.0, 0.0, 0.0, 0.0
        ])
    );

    let result = run("
        SELECT {[Entity].[US]} ON 0, {[Period].[Jan], [Period].[Mar]} ON 1,
               NON EMPTY {[Account].[Product Revenue], [Account].[Salaries]} ON AXIS(2)
        FROM [General Ledger] WHERE [Version].[Actual]");
    assert_eq!(names(&result, 2), ["Product Revenue"]);
    assert_eq!(numbers(&result), some(&[100.0, 0.0]));
}

#[test]
fn solve_order_decides_which_calculated_member_is_applied_last() {
    let query = |properties: &str| {
        format!(
            "
            WITH MEMBER [Account].[Share] AS [Account].[Product Revenue] / [Account].[Revenue],
                     FORMAT_STRING = '0.0%'
                 MEMBER [Version].[Change] AS [Version].[Actual] - [Version].[Budget]{properties}
            SELECT {{[Account].[Share]}} ON COLUMNS FROM [General Ledger]
            WHERE ([Entity].[US], [Period].[Jan], [Version].[Change])"
        )
    };
    // US January: product revenue 100 actual and 90 budget; revenue 130 actual and 90 budget.
    // Without a solve order the member of the earlier dimension is applied last: the share of
    // the changes. With one, the change is applied last: the change of the shares.
    assert_eq!(numbers(&run(&query(""))), some(&[10.0 / 40.0]));
    assert_eq!(
        numbers(&run(&query(", SOLVE_ORDER = 1"))),
        some(&[100.0 / 130.0 - 1.0])
    );
}

#[test]
fn text_cells_are_returned_compared_and_joined() {
    let definition = serde_json::from_value(serde_json::json!({
        "dimensions": [
            { "name": "Product", "elements": [
                { "name": "A", "type": "numeric" },
                { "name": "B", "type": "numeric" }
            ]},
            { "name": "Field", "elements": [
                { "name": "Units", "type": "numeric" },
                { "name": "Note", "type": "string" }
            ]}
        ],
        "cubes": [{ "name": "Stock", "dimensions": ["Product", "Field"] }]
    }))
    .unwrap();
    let mut database = Database::new(definition).unwrap();
    let write = |at: [&str; 2], value: Scalar| CellWrite {
        cube: "Stock".to_owned(),
        at: at.map(str::to_owned).to_vec(),
        value,
    };
    let writes = [
        write(["A", "Units"], Scalar::Number(5.0)),
        write(["A", "Note"], Scalar::Text("late".to_owned())),
    ];
    database.write(None, &writes).unwrap();

    let result = database
        .query(
            None,
            "
            WITH MEMBER [Field].[Label] AS [Product].CurrentMember.Name + ': ' + [Field].[Note]
            SELECT {[Field].[Units], [Field].[Note], [Field].[Label]} ON COLUMNS,
                   [Product].Members ON ROWS
            FROM [Stock]",
        )
        .unwrap();
    let text = |text: &str| Some(Scalar::Text(text.to_owned()));
    assert_eq!(
        result.cells,
        [
            Some(Scalar::Number(5.0)),
            text("late"),
            text("A: late"),
            Some(Scalar::Number(0.0)),
            text(""),
            text("B: ")
        ]
    );
    assert_eq!(result.axes[0].tuples[1][0].kind, MemberType::Text);

    let late = "SELECT NON EMPTY [Product].Members ON COLUMNS FROM [Stock] WHERE [Field].[Note]";
    assert_eq!(names(&database.query(None, late).unwrap(), 0), ["A"]);
    let filtered =
        "SELECT Filter([Product].Members, [Field].[Note] = 'late') ON COLUMNS FROM [Stock]";
    assert_eq!(names(&database.query(None, filtered).unwrap(), 0), ["A"]);
}

#[test]
fn comments_are_ignored() {
    let result = run("
        -- the two entities
        SELECT {[Entity].[US]} ON COLUMNS -- only one, in fact
        FROM [General Ledger]");
    assert_eq!(names(&result, 0), ["US"]);
}

#[test]
fn mistakes_are_reported() {
    let cases = [
        (
            "SELECT {[Entity].[US] ON COLUMNS FROM [General Ledger]",
            "expected ',' at character 22",
        ),
        (
            "SELECT {[Entity].[US} ON COLUMNS",
            "unclosed '[' at character 17",
        ),
        ("SELECT FROM", "expected a cube name at character 11"),
        (
            "SELECT FROM [General Ledger] extra",
            "unexpected text after the end of the query at character 29",
        ),
        (
            "SELECT {[Entity].[Mars]} ON COLUMNS FROM [General Ledger]",
            "dimension 'Entity' has no member 'Mars'",
        ),
        (
            "SELECT {[Colour].[Red]} ON COLUMNS FROM [General Ledger]",
            "the cube has no dimension 'Colour'",
        ),
        (
            "SELECT {[Entity].[US]} ON COLUMNS, {[Entity].[UK]} ON ROWS FROM [General Ledger]",
            "a dimension is used on more than one axis or in the slicer",
        ),
        (
            "SELECT {[Entity].[US]} ON COLUMNS FROM [General Ledger] WHERE [Entity].[UK]",
            "a dimension is used on more than one axis or in the slicer",
        ),
        (
            "SELECT {[Entity].[US], [Period].[Jan]} ON COLUMNS FROM [General Ledger]",
            "the tuples of a set must have the same dimensions",
        ),
        (
            "SELECT {[Entity].[US]} ON ROWS FROM [General Ledger]",
            "axes are numbered from COLUMNS without gaps or repeats",
        ),
        (
            "SELECT [Entity] ON COLUMNS FROM [General Ledger]",
            "expected a set; for every member of 'Entity' use .Members",
        ),
        (
            "SELECT Shuffle([Entity].Members) ON COLUMNS FROM [General Ledger]",
            "unknown function SHUFFLE",
        ),
        (
            "WITH MEMBER [Account].[Loop] AS [Account].[Loop] + 1
             SELECT {[Account].[Loop]} ON COLUMNS FROM [General Ledger]",
            "calculated member 'Loop' refers to itself",
        ),
        (
            "SELECT Filter([Entity].Members, 'yes' + 1 > 0) ON COLUMNS FROM [General Ledger]",
            "'yes' is not a number",
        ),
    ];
    for (mdx, expected) in cases {
        assert_eq!(failure(mdx), expected, "{mdx}");
    }

    let unknown_cube = database().query(None, "SELECT FROM [Nope]");
    assert!(matches!(
        unknown_cube,
        Err(Error::Cell(CellError::UnknownCube { .. }))
    ));
}

/// What an expression comes to as a calculated member, at US, February, Actual.
fn value(expression: &str) -> Option<Scalar> {
    let mdx = format!(
        "WITH MEMBER [Account].[X] AS {expression} SELECT {{[Account].[X]}} ON COLUMNS \
         FROM [General Ledger] WHERE ([Entity].[US], [Period].[Feb], [Version].[Actual])"
    );
    run(&mdx).cells[0].clone()
}

fn number(expression: &str) -> Option<f64> {
    match value(expression) {
        Some(Scalar::Number(number)) => Some(number),
        Some(Scalar::Text(text)) => panic!("expected a number, found '{text}'\n{expression}"),
        None => None,
    }
}

fn text(expression: &str) -> String {
    match value(expression) {
        Some(Scalar::Text(text)) => text,
        other => panic!("expected text, found {other:?}\n{expression}"),
    }
}

#[test]
fn members_are_reached_from_their_neighbours() {
    let revenue = |period: &str| number(&format!("([Account].[Product Revenue], {period})"));
    assert_eq!(revenue("[Period].CurrentMember.PrevMember"), Some(100.0));
    assert_eq!(revenue("[Period].CurrentMember.Lag(1)"), Some(100.0));
    assert_eq!(revenue("[Period].CurrentMember.NextMember"), Some(0.0));
    assert_eq!(revenue("[Period].[Mar].Lag(2)"), Some(100.0));
    assert_eq!(revenue("[Period].[Jan].Lead(1)"), Some(20.0));
    assert_eq!(revenue("[Period].[Mar].Lead(-2)"), Some(100.0));
    // Neighbours are of the same level: the quarter before Q2, not the month before it.
    assert_eq!(text("[Period].[Q2].PrevMember.Name"), "Q1");
    assert_eq!(text("[Period].[Jun].PrevMember.Name"), "May");
    // Past either end there is no member, and so no cell.
    assert_eq!(revenue("[Period].[Jan].PrevMember"), None);
    assert_eq!(revenue("[Period].[Jun].Lead(3)"), None);
    assert_eq!(revenue("[Period].[H1].NextMember"), None);

    // The change on the month before, down a column of months.
    let result = run(
        "WITH MEMBER [Version].[Change] AS [Version].[Actual] - ([Version].[Actual], [Period].CurrentMember.PrevMember) \
         SELECT {[Period].[Jan], [Period].[Feb], [Period].[Mar]} ON COLUMNS FROM [General Ledger] \
         WHERE ([Account].[Product Revenue], [Entity].[US], [Version].[Change])",
    );
    assert_eq!(numbers(&result), [Some(100.0), Some(-80.0), Some(-20.0)]);
    assert!(
        failure("SELECT {[Period].[Jan].Lag()} ON COLUMNS FROM [General Ledger]")
            .contains(".LAG takes one distance")
    );
}

#[test]
fn rank_places_a_tuple_in_a_set() {
    let entities = "{[Entity].[US], [Entity].[UK]}";
    assert_eq!(
        number(&format!("Rank([Entity].[UK], {entities})")),
        Some(2.0)
    );
    assert_eq!(
        number(&format!("Rank([Entity].[US], {entities})")),
        Some(1.0)
    );
    assert_eq!(
        number(&format!("Rank([Entity].[Total Entity], {entities})")),
        Some(0.0)
    );
    // By value the greatest is first: in January the US sold 100 and the UK 50.
    let by = |entity: &str| {
        number(&format!(
            "Rank([Entity].[{entity}], [Entity].Members, ([Account].[Product Revenue], [Period].[Jan]))"
        ))
    };
    assert_eq!(by("Total Entity"), Some(1.0));
    assert_eq!(by("US"), Some(2.0));
    assert_eq!(by("UK"), Some(3.0));
    // Equal values share a rank: in March nobody sold anything.
    let march =
        "Rank([Entity].[UK], [Entity].Members, ([Account].[Product Revenue], [Period].[Mar]))";
    assert_eq!(number(march), Some(1.0));
}

#[test]
fn text_is_read_as_a_member_a_tuple_or_a_set() {
    assert_eq!(
        number("(StrToMember('[Account].[Product Revenue]'), [Period].[Jan])"),
        Some(100.0)
    );
    assert_eq!(
        number("StrToTuple('([Account].[Product Revenue], [Period].[Jan], [Entity].[UK])')"),
        Some(50.0)
    );
    assert_eq!(
        number("Sum(StrToSet('{[Period].[Jan], [Period].[Feb]}'), [Account].[Product Revenue])"),
        Some(120.0)
    );
    // The text may be put together: the month before this one, by name.
    assert_eq!(
        number(
            "(StrToMember('[Period].[' + [Period].CurrentMember.PrevMember.Name + ']'), [Account].[Product Revenue])"
        ),
        Some(100.0)
    );
    let mistake = failure(
        "WITH MEMBER [Account].[X] AS StrToMember('[Period].Members') \
         SELECT {[Account].[X]} ON COLUMNS FROM [General Ledger]",
    );
    assert!(
        mistake.contains("STRTOMEMBER: '[Period].Members' is not one"),
        "{mistake}"
    );
}

#[test]
fn numbers_are_rounded_rooted_and_raised() {
    assert_eq!(number("Abs(-2.5)"), Some(2.5));
    assert_eq!(number("Round(2.567, 2)"), Some(2.57));
    assert_eq!(number("Round(2.5)"), Some(3.0));
    assert_eq!(number("Round(-2.5)"), Some(-3.0));
    assert_eq!(number("Round(1234, -2)"), Some(1200.0));
    assert_eq!(number("Int(-2.5)"), Some(-3.0));
    assert_eq!(number("Fix(-2.5)"), Some(-2.0));
    assert_eq!(number("Sqr(16)"), Some(4.0));
    assert_eq!(number("Power(2, 10)"), Some(1024.0));
    assert_eq!(number("Mod(7, 3)"), Some(1.0));
    assert_eq!(number("Mod(-7, 3)"), Some(2.0));
    assert_eq!(number("Sgn(-3) + Sgn(0) + Sgn(8) * 10"), Some(9.0));
    assert_eq!(number("Round(Log(Exp(2)), 9)"), Some(2.0));
    assert_eq!(number("Round(Log10(1000), 9)"), Some(3.0));
    // What does not exist is no value, as with a division by zero.
    assert_eq!(number("Sqr(-1)"), None);
    assert_eq!(number("Log(0)"), None);
    assert_eq!(number("Mod(1, 0)"), None);
    // They work on cells as on numbers.
    assert_eq!(
        number("Round(([Account].[Product Revenue], [Period].[Jan]) / 3, 1)"),
        Some(33.3)
    );
    // A number may be written with an exponent, but not one too large to hold.
    assert_eq!(number("1.5E3 + 25e-2 + 1E+2"), Some(1600.25));
    assert_eq!(refusal("1e999"), "malformed number at character 29");
}

#[test]
fn text_is_measured_cut_and_changed() {
    assert_eq!(number("Len('hello')"), Some(5.0));
    assert_eq!(text("Left('hello', 2)"), "he");
    assert_eq!(text("Right('hello', 3)"), "llo");
    assert_eq!(text("Left('hello', 9)"), "hello");
    assert_eq!(text("Mid('hello', 2, 3)"), "ell");
    assert_eq!(text("Mid('hello', 4)"), "lo");
    assert_eq!(text("UCase('Hello')"), "HELLO");
    assert_eq!(text("LCase('Hello')"), "hello");
    assert_eq!(text("Trim('  a b  ')"), "a b");
    assert_eq!(number("InStr('hello', 'll')"), Some(3.0));
    assert_eq!(number("InStr('hello', 'z')"), Some(0.0));
    assert_eq!(text("Replace('a-b-c', '-', '+')"), "a+b+c");
    assert_eq!(number("Val('12.5kg')"), Some(12.5));
    assert_eq!(number("Val('kg')"), Some(0.0));
    assert_eq!(text("Str(12)"), "12");
    assert_eq!(text("CStr(1.5)"), "1.5");
    // Characters are counted, not bytes.
    assert_eq!(number("Len('héllo')"), Some(5.0));
    assert_eq!(text("Mid('héllo', 2, 2)"), "él");
    // On the names of members, which is where they are mostly wanted.
    assert_eq!(text("UCase(Left([Period].CurrentMember.Name, 2))"), "FE");
    assert_eq!(number("Len([Period].CurrentMember.Parent.Name)"), Some(2.0));
}

/// What an expression that should fail says, as a calculated member.
fn refusal(expression: &str) -> String {
    failure(&format!(
        "WITH MEMBER [Account].[X] AS {expression} SELECT {{[Account].[X]}} ON COLUMNS \
         FROM [General Ledger]"
    ))
}

#[test]
fn dates_are_days_from_the_end_of_1899_as_excel_counts_them() {
    // Sunday 22 November 2026.
    let day = 46_348.0;
    assert_eq!(number("DateSerial(2026, 11, 22)"), Some(day));
    assert_eq!(number("DateSerial(1899, 12, 30)"), Some(0.0));
    assert_eq!(number("DateSerial(1900, 3, 1)"), Some(61.0));
    // Months and days run over, so the day before the 1st is the last of the month before.
    assert_eq!(number("DateSerial(2026, 13, 1)"), Some(46_388.0));
    assert_eq!(number("Day(DateSerial(2026, 3, 0))"), Some(28.0));
    assert_eq!(number("Day(DateSerial(2024, 3, 0))"), Some(29.0));
    // Taken apart, from a number or from a text written as yyyy-mm-dd.
    let evening = "CDate('2026-11-22 18:05:09')";
    assert_eq!(
        number(evening),
        Some(day + (18.0 * 3600.0 + 309.0) / 86_400.0)
    );
    assert_eq!(
        number("CDate('2026-11-22T18:05')"),
        Some(day + (18.0 * 60.0 + 5.0) / 1_440.0)
    );
    for (part, expected) in [
        ("Year", 2026.0),
        ("Month", 11.0),
        ("Day", 22.0),
        ("Hour", 18.0),
        ("Minute", 5.0),
        ("Second", 9.0),
        ("Weekday", 1.0),
    ] {
        assert_eq!(
            number(&format!("{part}({evening})")),
            Some(expected),
            "{part}"
        );
    }
    assert_eq!(number("Year('2026-11-22')"), Some(2026.0));
    assert_eq!(number("Weekday(DateSerial(2026, 11, 22), 2)"), Some(7.0));
    // Moved, a month on from the 31st being the last day of a shorter month.
    assert_eq!(
        number("DateAdd('m', 1, DateSerial(2026, 1, 31))"),
        Some(46_081.0)
    );
    assert_eq!(
        number("DateAdd('yyyy', 1, DateSerial(2024, 2, 29))"),
        Some(45_716.0)
    );
    assert_eq!(
        number("DateAdd('q', -1, DateSerial(2026, 11, 22))"),
        Some(46_256.0)
    );
    assert_eq!(
        number("DateAdd('d', -1, DateSerial(2026, 3, 1))"),
        Some(46_081.0)
    );
    assert_eq!(
        number("DateAdd('ww', 2, DateSerial(2026, 11, 22))"),
        Some(day + 14.0)
    );
    assert_eq!(
        number("DateAdd('H', 30, DateSerial(2026, 11, 22))"),
        Some(day + 1.25)
    );
    // Compared by the boundaries passed, as VBA compares them.
    let between = |interval: &str, from: &str, to: &str| {
        number(&format!(
            "DateDiff('{interval}', CDate('{from}'), CDate('{to}'))"
        ))
    };
    assert_eq!(between("yyyy", "2025-12-31", "2026-01-01"), Some(1.0));
    assert_eq!(between("q", "2026-03-31", "2026-04-01"), Some(1.0));
    assert_eq!(between("m", "2026-01-31", "2026-02-01"), Some(1.0));
    assert_eq!(between("d", "2026-11-22", "2026-11-01"), Some(-21.0));
    assert_eq!(between("ww", "2026-11-21", "2026-11-22"), Some(1.0));
    assert_eq!(between("w", "2026-11-21", "2026-11-22"), Some(0.0));
    assert_eq!(between("w", "2026-11-01", "2026-11-22"), Some(3.0));
    assert_eq!(
        between("h", "2026-11-22 23:59", "2026-11-23 00:01"),
        Some(1.0)
    );
    assert_eq!(
        between("n", "2026-11-22 10:00", "2026-11-22 08:30"),
        Some(-90.0)
    );
    // Now is the server's clock, and Date its day.
    assert_eq!(
        number("IIF(Now() > DateSerial(2026, 1, 1), 1, 0)"),
        Some(1.0)
    );
    assert_eq!(
        number("IIF(Date() <= Now() AND Now() - Date() < 1, 1, 0)"),
        Some(1.0)
    );

    for (expression, expected) in [
        (
            "DateSerial(10000, 1, 1)",
            "a date falls in the years 100 to 9999",
        ),
        (
            "DateAdd('yyyy', 1000000000000000, 0)",
            "a date falls in the years 100 to 9999",
        ),
        ("Year(10000000)", "a date falls in the years 100 to 9999"),
        (
            "DateAdd('x', 1, 0)",
            "'x' is no interval: yyyy, q, m, y, d, w, ww, h, n or s",
        ),
        (
            "CDate('22/11/2026')",
            "'22/11/2026' is not a date written as yyyy-mm-dd",
        ),
        (
            "CDate('2026-02-30')",
            "'2026-02-30' is not a date written as yyyy-mm-dd",
        ),
        (
            "Weekday(0, 8)",
            "a week begins on a day from 1, Sunday, to 7, Saturday",
        ),
    ] {
        assert_eq!(refusal(expression), expected, "{expression}");
    }
}

#[test]
fn numbers_and_dates_are_written_out_as_a_pattern_says() {
    let written = |value: &str, pattern: &str| text(&format!("Format({value}, '{pattern}')"));
    let evening = "CDate('2026-11-22 18:05:09')";
    assert_eq!(written(evening, "yyyy-mm-dd"), "2026-11-22");
    assert_eq!(written(evening, "mmmm yyyy"), "November 2026");
    assert_eq!(written(evening, "dddd d mmm yy"), "Sunday 22 Nov 26");
    assert_eq!(written(evening, "ddd dd/mm"), "Sun 22/11");
    assert_eq!(written(evening, "hh:mm:ss"), "18:05:09");
    assert_eq!(written(evening, "h:nn AM/PM"), "6:05 PM");
    assert_eq!(written(evening, "h:nn a/p"), "6:05 p");
    assert_eq!(written(evening, "q y ww w"), "4 326 48 1");
    assert_eq!(written(evening, r#""Q"q \y yyyy"#), "Q4 y 2026");
    // A text written as a date is written out again.
    assert_eq!(written("'2026-11-22'", "mmm d"), "Nov 22");

    assert_eq!(written("1234.5", "#,##0.00"), "1,234.50");
    assert_eq!(written("1234567", r##"#,##0,"K""##), "1,235K");
    assert_eq!(written("0.256", "0.0%"), "25.6%");
    assert_eq!(written("2.5", "0"), "3");
    assert_eq!(written("5", "000"), "005");
    assert_eq!(written("0.5", "#.##"), ".5");
    assert_eq!(written("1", "#.##"), "1.");
    assert_eq!(written("-5", "0"), "-5");
    assert_eq!(written("-5", "0;(0)"), "(5)");
    assert_eq!(written("0", r#"0;(0);"none""#), "none");
    assert_eq!(written("-0.004", "0.00"), "0.00");
    assert_eq!(written("'12.5'", "0.0"), "12.5");
    assert_eq!(written("[Account].[Product Revenue]", "$#,##0"), "$20");
    assert_eq!(text("Format(5)"), "5");
    assert_eq!(text("Format(5, '')"), "5");
    // A power of ten after the digits: E+ writes its sign always, E- only when negative.
    assert_eq!(written("1234.5", "0.00E+00"), "1.23E+03");
    assert_eq!(written("0.000123", "0.0E-0"), "1.2E-4");
    assert_eq!(written("123", "0.0E-0"), "1.2E2");
    assert_eq!(written("9.99", "0.0e+0"), "1.0e+1");
    assert_eq!(written("1234", "00.0E+0"), "12.3E+2");
    assert_eq!(written("0", "0.00E+00"), "0.00E+00");
    assert_eq!(written("-1234.5", "Scientific"), "-1.23E+03");
    // VBA's formats by name that read the same in every country.
    assert_eq!(written("1234.5", "Standard"), "1,234.50");
    assert_eq!(written("2", "fixed"), "2.00");
    assert_eq!(written("0.1234", "Percent"), "12.34%");
    assert_eq!(written("1.5", "General Number"), "1.5");
    assert_eq!(written("0", "Yes/No"), "No");
    assert_eq!(written("-3", "Yes/No"), "Yes");
    assert_eq!(written("1", "True/False"), "True");
    assert_eq!(written("0", "On/Off"), "Off");
    // An empty cell is zero, as everywhere; only no value at all is written as nothing.
    assert_eq!(written("[Account].[Rent]", "0.00"), "0.00");
    assert_eq!(written("NULL", "0.00"), "");

    for (expression, expected) in [
        ("Format('abc', '0')", "'abc' is not a number"),
        (
            "Format(1, '0 x 0')",
            "a number pattern has its text before or after its digits, not among them",
        ),
        (
            "Format(1, '0.000000000000000000000')",
            "a number pattern writes at most 20 decimals",
        ),
    ] {
        assert_eq!(refusal(expression), expected, "{expression}");
    }
}

#[test]
fn a_calculated_member_may_say_how_its_cells_are_written_out() {
    let written = |mdx: &str| run(mdx).formatted;
    let owned = |texts: &[Option<&str>]| -> Vec<Option<String>> {
        texts.iter().map(|text| text.map(str::to_owned)).collect()
    };
    // Revenue at US, January is 130, of which 100 is product revenue.
    let share = written(
        "WITH MEMBER [Account].[Share] AS [Account].[Product Revenue] / [Account].[Revenue],
                 FORMAT_STRING = '0.0%'
              MEMBER [Account].[Plain] AS 1234.5
         SELECT {[Account].[Product Revenue], [Account].[Share], [Account].[Plain]} ON COLUMNS
         FROM [General Ledger] WHERE ([Entity].[US], [Period].[Jan], [Version].[Actual])",
    );
    assert_eq!(share, owned(&[None, Some("76.9%"), None]));
    // Of two calculated members at a cell, the one worked out last decides.
    let half = written(
        "WITH MEMBER [Account].[Double] AS [Account].[Product Revenue] * 2, FORMAT_STRING = '0.00'
              MEMBER [Version].[Half] AS [Version].[Actual] / 2,
                 FORMAT_STRING = '#,##0 \"half\"', SOLVE_ORDER = 1
         SELECT {[Account].[Double]} ON COLUMNS, {[Version].[Actual], [Version].[Half]} ON ROWS
         FROM [General Ledger] WHERE ([Entity].[US], [Period].[Jan])",
    );
    assert_eq!(half, owned(&[Some("200.00"), Some("100 half")]));
    // A pattern may be worked out at each cell, and cells NON EMPTY drops go with theirs.
    let thirds = written(
        "WITH MEMBER [Account].[Third] AS [Account].[Product Revenue] / 3,
                 FORMAT_STRING = IIF([Period].CurrentMember.Name = 'Jan', '0', '0.0')
         SELECT {[Account].[Third]} ON COLUMNS,
                NON EMPTY {[Period].[Jan], [Period].[Mar], [Period].[Feb]} ON ROWS
         FROM [General Ledger] WHERE ([Entity].[US], [Version].[Actual])",
    );
    assert_eq!(thirds, owned(&[Some("33"), Some("6.7")]));
    // A date is written out as one; a number its pattern cannot write out is left as it is.
    let days = written(
        "WITH MEMBER [Account].[Day] AS DateSerial(2026, 11, 22), FORMAT_STRING = 'dddd'
              MEMBER [Account].[Far] AS 10000000, FORMAT_STRING = 'yyyy'
         SELECT {[Account].[Day], [Account].[Far]} ON COLUMNS FROM [General Ledger]",
    );
    assert_eq!(days, owned(&[Some("Sunday"), None]));
    // Without a format, nothing is written out, and the answer says nothing of it.
    let plain = run("SELECT {[Account].[Rent]} ON COLUMNS FROM [General Ledger]");
    assert!(plain.formatted.is_empty());
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("formatted")
            .is_none()
    );
    // A pattern that could write nothing out is refused before any cell is worked out.
    assert_eq!(
        failure(
            "WITH MEMBER [Account].[Bad] AS 1, FORMAT_STRING = '0 x 0'
             SELECT {[Account].[Bad]} ON COLUMNS FROM [General Ledger]"
        ),
        "the FORMAT_STRING of [Account].[Bad]: a number pattern has its text before or after \
         its digits, not among them"
    );
}

#[test]
fn levels_count_up_from_the_leaves() {
    let columns = |set: &str| {
        let mdx = format!("SELECT {set} ON COLUMNS FROM [General Ledger]");
        names(&run(&mdx), 0)
    };
    assert_eq!(columns("{[Period].Levels(1).Members}"), ["Q1", "Q2"]);
    assert_eq!(columns("[Period].Levels(2).Members"), ["H1"]);
    assert_eq!(
        columns("[Period].[Feb].Level.Members"),
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun"]
    );
    assert_eq!(columns("[Period].Levels(7).Members"), [""; 0]);
    assert_eq!(number("[Period].[Q1].Level.Ordinal"), Some(1.0));
    assert_eq!(number("[Period].CurrentMember.Level.Ordinal"), Some(0.0));
    // An ancestor is found by level as well as by distance.
    assert_eq!(
        text("Ancestor([Period].[May], [Period].Levels(2)).Name"),
        "H1"
    );
    assert_eq!(
        text("Ancestor([Period].[May], [Period].[Q1].Level).Name"),
        "Q2"
    );
    assert_eq!(text("Ancestor([Period].[May], 1).Name"), "Q2");
    let mistake = failure(
        "SELECT {Ancestor([Period].[H1], [Period].Levels(0))} ON COLUMNS FROM [General Ledger]",
    );
    assert!(
        mistake.contains("member 'H1' has no ancestor at level 0"),
        "{mistake}"
    );
}

#[test]
fn a_member_has_cousins_and_parallel_periods() {
    assert_eq!(text("Cousin([Period].[Feb], [Period].[Q2]).Name"), "May");
    assert_eq!(text("Cousin([Period].[Jun], [Period].[Q1]).Name"), "Mar");
    // Under its own ancestor a member is its own cousin.
    assert_eq!(text("Cousin([Period].[Q2], [Period].[H1]).Name"), "Q2");
    // The same month of the quarter before, or of the quarter after.
    let parallel = |arguments: &str| format!("ParallelPeriod([Period].Levels(1){arguments})");
    assert_eq!(
        text(&format!("{}.Name", parallel(", 1, [Period].[May]"))),
        "Feb"
    );
    assert_eq!(
        text(&format!("{}.Name", parallel(", -1, [Period].[Jan]"))),
        "Apr"
    );
    assert_eq!(
        text(&format!("{}.Name", parallel(", 0, [Period].[Jan]"))),
        "Jan"
    );
    // With nothing else said: one back from the current member, here February.
    let revenue = |period: &str| number(&format!("([Account].[Product Revenue], {period})"));
    assert_eq!(revenue(&parallel(", 1, [Period].[May]")), Some(20.0));
    assert_eq!(revenue(&parallel("")), None);
    assert_eq!(revenue(&parallel(", -1")), Some(0.0));
    let mistake = failure("SELECT {ParallelPeriod(1)} ON COLUMNS FROM [General Ledger]");
    assert!(mistake.contains("expected a level"), "{mistake}");
}

#[test]
fn periods_run_to_date_within_an_ancestor() {
    let columns = |set: &str| {
        let mdx = format!("SELECT {set} ON COLUMNS FROM [General Ledger]");
        names(&run(&mdx), 0)
    };
    let quarter = "PeriodsToDate([Period].Levels(1), [Period].[May])";
    assert_eq!(columns(quarter), ["Apr", "May"]);
    let half = "PeriodsToDate([Period].Levels(2), [Period].[May])";
    assert_eq!(columns(half), ["Jan", "Feb", "Mar", "Apr", "May"]);
    assert_eq!(
        columns("PeriodsToDate([Period].Levels(2), [Period].[Q2])"),
        ["Q1", "Q2"]
    );
    assert_eq!(
        columns("PeriodsToDate([Period].Levels(0), [Period].[May])"),
        ["May"]
    );
    // The half year to date, at the current member: January and February.
    assert_eq!(
        number("Sum(PeriodsToDate([Period].Levels(2)), [Account].[Product Revenue])"),
        Some(120.0)
    );
}

#[test]
fn members_are_opened_and_closed_in_place() {
    let columns = |set: &str| {
        let mdx = format!("SELECT {set} ON COLUMNS FROM [General Ledger]");
        names(&run(&mdx), 0)
    };
    let quarters = ["H1", "Q1", "Q2"];
    let opened = ["H1", "Q1", "Jan", "Feb", "Mar", "Q2"];
    let all = ["H1", "Q1", "Jan", "Feb", "Mar", "Q2", "Apr", "May", "Jun"];
    let down = |arguments: &str| columns(&format!("DrilldownMember({arguments})"));
    assert_eq!(down("{[Period].[H1]}, {[Period].[H1]}"), quarters);
    assert_eq!(
        down("{[Period].[H1], [Period].[Q1], [Period].[Q2]}, {[Period].[Q1]}"),
        opened
    );
    // The children put in are opened too only when asked.
    let both = "{[Period].[H1]}, {[Period].[H1], [Period].[Q1]}";
    assert_eq!(down(both), quarters);
    assert_eq!(down(&format!("{both}, RECURSIVE")), opened);
    // A member that is not in the set stays as it is, and so does a leaf.
    assert_eq!(down("{[Period].[H1]}, {[Period].[Q1]}"), ["H1"]);
    assert_eq!(down("{[Period].[Jan]}, {[Period].[Jan]}"), ["Jan"]);
    assert_eq!(
        columns("TM1DrilldownMember({[Period].[H1]}, ALL, RECURSIVE)"),
        all
    );
    assert_eq!(
        columns("TM1DrilldownMember({[Period].[H1]}, ALL)"),
        quarters
    );
    assert_eq!(
        columns("TM1DrilldownMember({[Period].[H1]}, {[Period].[H1]})"),
        quarters
    );

    // In a tuple the child takes its parent's place, whichever place that is.
    let result = run(
        "SELECT DrilldownMember({[Entity].[US]} * {[Period].[Q1], [Period].[Q2]}, {[Period].[Q2]}) \
         ON COLUMNS FROM [General Ledger]",
    );
    assert_eq!(result.axes[0].tuples.len(), 5);
    assert_eq!(result.axes[0].tuples[2][1].name, "Apr");
    assert_eq!(result.axes[0].tuples[2][0].name, "US");

    // A level is opened where it is deepest, or where it is named.
    assert_eq!(columns("DrilldownLevel({[Period].[H1]})"), quarters);
    assert_eq!(
        columns("DrilldownLevel(DrilldownLevel({[Period].[H1]}))"),
        all
    );
    assert_eq!(
        columns(
            "DrilldownLevel({[Period].[H1], [Period].[Q1], [Period].[Q2]}, [Period].Levels(2))"
        ),
        ["H1", "Q1", "Q2", "Q1", "Q2"]
    );

    // Closing a member takes away what follows beneath it, and nothing else.
    let up = |members: &str| {
        columns(&format!(
            "DrillupMember(TM1DrilldownMember({{[Period].[H1]}}, ALL, RECURSIVE), {members})"
        ))
    };
    assert_eq!(
        up("{[Period].[Q1]}"),
        ["H1", "Q1", "Q2", "Apr", "May", "Jun"]
    );
    assert_eq!(up("{[Period].[H1]}"), ["H1"]);
    assert_eq!(up("{[Period].[Jan]}"), all);
    let mistake = failure(
        "SELECT DrilldownMember({[Period].[H1]}, {[Period].[H1]}, DEEP) ON COLUMNS FROM [General Ledger]",
    );
    assert!(
        mistake.contains("unknown DRILLDOWNMEMBER flag DEEP"),
        "{mistake}"
    );
}

#[test]
fn periods_are_counted_back_and_found_at_either_end() {
    let columns = |set: &str| {
        let mdx = format!("SELECT {set} ON COLUMNS FROM [General Ledger]");
        names(&run(&mdx), 0)
    };
    assert_eq!(
        columns("LastPeriods(3, [Period].[May])"),
        ["Mar", "Apr", "May"]
    );
    assert_eq!(columns("LastPeriods(-2, [Period].[May])"), ["May", "Jun"]);
    assert_eq!(columns("LastPeriods(1, [Period].[Q2])"), ["Q2"]);
    // Where the level ends first there are fewer, and none at all of no periods.
    assert_eq!(columns("LastPeriods(5, [Period].[Feb])"), ["Jan", "Feb"]);
    assert!(columns("LastPeriods(0, [Period].[Feb])").is_empty());
    let mistake = failure("SELECT LastPeriods(2) ON COLUMNS FROM [General Ledger]");
    assert!(mistake.contains("dimension of type 'time'"), "{mistake}");

    assert_eq!(
        text("OpeningPeriod([Period].Levels(0), [Period].[Q2]).Name"),
        "Apr"
    );
    assert_eq!(
        text("ClosingPeriod([Period].Levels(0), [Period].[H1]).Name"),
        "Jun"
    );
    assert_eq!(
        text("ClosingPeriod([Period].Levels(1), [Period].[H1]).Name"),
        "Q2"
    );
    assert_eq!(
        text("OpeningPeriod([Period].Levels(1), [Period].[Q2]).Name"),
        "Q2"
    );
    // The opening balance of a half is that of its first month.
    assert_eq!(
        number("([Account].[Product Revenue], OpeningPeriod([Period].Levels(0), [Period].[H1]))"),
        Some(100.0)
    );
    // Nothing opens a month at the level of the quarters above it.
    assert_eq!(
        number("([Account].[Product Revenue], OpeningPeriod([Period].Levels(1), [Period].[Jan]))"),
        None
    );

    assert_eq!(text("[Period].[May].FirstSibling.Name"), "Apr");
    assert_eq!(text("[Period].[May].LastSibling.Name"), "Jun");
    assert_eq!(text("[Period].[Q1].LastSibling.Name"), "Q2");
}

#[test]
fn a_value_is_chosen_case_by_case() {
    let revenue = "[Account].[Product Revenue]";
    let size = |period: &str| {
        text(&format!(
            "CASE WHEN ({revenue}, {period}) >= 100 THEN 'large' \
             WHEN ({revenue}, {period}) > 0 THEN 'small' ELSE 'none' END"
        ))
    };
    assert_eq!(size("[Period].[Jan]"), "large");
    assert_eq!(size("[Period].[Feb]"), "small");
    assert_eq!(size("[Period].[Mar]"), "none");
    // A value is set against what it may equal, and the words may be in any case.
    let half = "case [Period].CurrentMember.Name when 'Jan' then 1 when 'Feb' then 2 end";
    assert_eq!(number(half), Some(2.0));
    // Without an ELSE, a value that meets no case is nothing.
    assert_eq!(
        number("CASE [Period].CurrentMember.Name WHEN 'Jan' THEN 1 END"),
        None
    );
    assert_eq!(number("CASE WHEN 1 = 1 THEN 2 END + 1"), Some(3.0));
    assert_eq!(number("IIF(1 = 2, 5, NULL)"), None);

    // IS asks whether two are the same member, where = compares their cells.
    let is = |condition: &str| text(&format!("IIF({condition}, 'yes', 'no')"));
    assert_eq!(is("[Period].CurrentMember IS [Period].[Feb]"), "yes");
    assert_eq!(is("[Period].CurrentMember IS [Period].[Jan]"), "no");
    assert_eq!(is("[Period].[Jan].PrevMember IS NULL"), "yes");
    assert_eq!(is("[Period].[Feb].PrevMember IS NULL"), "no");
    assert_eq!(is("NOT [Period].CurrentMember IS [Period].[Jan]"), "yes");
    assert_eq!(is("[Period].[Feb].Level IS [Period].Levels(0)"), "yes");
    assert_eq!(is("1 = 1 XOR 1 = 2"), "yes");
    assert_eq!(is("1 = 1 XOR 2 = 2"), "no");

    assert_eq!(is("IsAncestor([Period].[H1], [Period].[Feb])"), "yes");
    assert_eq!(is("IsAncestor([Period].[Q2], [Period].[Feb])"), "no");
    assert_eq!(is("IsAncestor([Period].[Feb], [Period].[Feb])"), "no");
    assert_eq!(is("IsGeneration([Period].[Feb], 0)"), "yes");
    assert_eq!(is("IsGeneration([Period].[Q1], 1)"), "yes");
    assert_eq!(is("IsGeneration([Period].[Q1], 0)"), "no");
    assert_eq!(is("IsSibling([Period].[Jan], [Period].[Feb])"), "yes");
    assert_eq!(is("IsSibling([Period].[Jan], [Period].[Apr])"), "no");
    assert_eq!(is("IsSibling([Period].[Jan], [Period].[Jan])"), "no");
    assert_eq!(
        is("IsSibling([Period].[Jan], [Period].[Jan], INCLUDEMEMBER)"),
        "yes"
    );
    assert_eq!(
        columns("Unorder({[Period].[Feb], [Period].[Jan]})"),
        ["Feb", "Jan"]
    );
    assert_eq!(columns("Ascendants([Period].[Feb])"), ["Feb", "Q1", "H1"]);
    assert_eq!(text("[Period].[Feb].Caption"), "Feb");
    // Written as MDX, and read back.
    assert_eq!(text("MemberToStr([Period].[Feb])"), "[Period].[Feb]");
    assert_eq!(
        text("TupleToStr(([Period].[Feb], [Account].[Rent]))"),
        "([Period].[Feb], [Account].[Rent])"
    );
    assert_eq!(
        text("SetToStr({[Period].[Jan], [Period].[Feb]})"),
        "{[Period].[Jan], [Period].[Feb]}"
    );
    assert_eq!(
        columns("StrToSet(SetToStr({[Period].[Jan], [Period].[Feb]}))"),
        ["Jan", "Feb"]
    );
    assert_eq!(columns("NameToSet(\"[Period].[Feb]\")"), ["Feb"]);
    // Drilled down to the children with the largest values, or the smallest, an argument
    // left out between commas.
    let q2 = "IIF([Period].CurrentMember IS [Period].[Q2], 1, 0)";
    assert_eq!(
        columns(&format!("DrillDownLevelTop({{[Period].[H1]}}, 1, , {q2})")),
        ["H1", "Q2"]
    );
    assert_eq!(
        columns(&format!(
            "DrillDownLevelBottom({{[Period].[H1]}}, 1, , {q2})"
        )),
        ["H1", "Q1"]
    );
    let mar = "IIF([Period].CurrentMember IS [Period].[Mar], 1, 0)";
    assert_eq!(
        columns(&format!(
            "DrillDownMemberTop({{[Period].[H1], [Period].[Q1]}}, {{[Period].[Q1]}}, 2, {mar})"
        )),
        ["H1", "Q1", "Mar", "Jan"]
    );
    // Drilled up: without the members below a level, or those furthest beneath the top.
    assert_eq!(
        columns("DrillUpLevel({[Period].[H1], [Period].[Q1], [Period].[Jan], [Period].[Feb]})"),
        ["H1", "Q1"]
    );
    assert_eq!(
        columns("DrillUpLevel({[Period].[H1], [Period].[Q1], [Period].[Jan]}, [Period].Levels(2))"),
        ["H1"]
    );
    assert_eq!(number("StrToValue(\"1 + 2\")"), Some(3.0));
    assert_eq!(text("[Period].DefaultMember.Name"), "H1");

    for (mdx, expected) in [
        ("CASE 1 THEN 2 END", "expected WHEN"),
        ("CASE WHEN 1 = 1 THEN 2", "expected END"),
        ("CASE WHEN 1 = 1 2 END", "expected THEN"),
    ] {
        let mistake = failure(&format!(
            "WITH MEMBER [Account].[X] AS {mdx} SELECT {{[Account].[X]}} ON COLUMNS FROM [General Ledger]"
        ));
        assert!(mistake.contains(expected), "{mistake}");
    }
}

#[test]
fn a_member_is_opened_if_shut_and_shut_if_open() {
    let columns = |set: &str| {
        let mdx = format!("SELECT {set} ON COLUMNS FROM [General Ledger]");
        names(&run(&mdx), 0)
    };
    let shut = "{[Period].[H1], [Period].[Q1], [Period].[Q2]}";
    let open = ["H1", "Q1", "Jan", "Feb", "Mar", "Q2"];
    let toggled = format!("ToggleDrillState({shut}, {{[Period].[Q1]}})");
    assert_eq!(columns(&toggled), open);
    assert_eq!(
        columns(&format!("ToggleDrillState({toggled}, {{[Period].[Q1]}})")),
        ["H1", "Q1", "Q2"]
    );
    // Each member chosen goes its own way.
    assert_eq!(
        columns(&format!(
            "ToggleDrillState({toggled}, {{[Period].[Q1], [Period].[Q2]}})"
        )),
        ["H1", "Q1", "Q2", "Apr", "May", "Jun"]
    );
    assert_eq!(
        columns(&format!("ToggleDrillState({shut}, {{[Period].[H1]}})")),
        ["H1"]
    );
}

#[test]
fn a_cross_join_may_leave_out_its_empty_tuples() {
    let sets = "{[Entity].[US], [Entity].[UK]}, [Period].Members";
    let slicer = "WHERE ([Account].[Product Revenue], [Version].[Actual])";
    let tuples = |set: &str| {
        let result = run(&format!(
            "SELECT {set} ON COLUMNS FROM [General Ledger] {slicer}"
        ));
        let names = |tuple: &Vec<_>| -> Vec<String> {
            tuple
                .iter()
                .map(|member: &epiphany_core::mdx::MemberResult| member.name.clone())
                .collect()
        };
        result.axes[0].tuples.iter().map(names).collect::<Vec<_>>()
    };
    let kept = tuples(&format!("NonEmptyCrossJoin({sets})"));
    assert_eq!(kept, tuples(&format!("NonEmpty(CrossJoin({sets}))")));
    assert!(kept.contains(&vec!["US".to_owned(), "Jan".to_owned()]));
    assert!(kept.len() < tuples(&format!("CrossJoin({sets})")).len());
}

#[test]
fn calculated_members_are_added_to_a_set_and_taken_from_it() {
    let columns = |set: &str| {
        let mdx = format!(
            "WITH MEMBER [Period].[Best] AS 1 MEMBER [Entity].[Other] AS 2 \
             SELECT {set} ON COLUMNS FROM [General Ledger]"
        );
        names(&run(&mdx), 0)
    };
    let added = "AddCalculatedMembers({[Period].[Jan], [Period].[Feb]})";
    assert_eq!(columns(added), ["Jan", "Feb", "Best"]);
    // One that is there already is not added again.
    assert_eq!(
        columns("AddCalculatedMembers({[Period].[Best], [Period].[Jan]})"),
        ["Best", "Jan"]
    );
    assert_eq!(
        columns(&format!("StripCalculatedMembers({added})")),
        ["Jan", "Feb"]
    );
    assert_eq!(
        columns("StripCalculatedMembers([Period].AllMembers)"),
        columns("[Period].Members")
    );
}

#[test]
fn a_set_too_large_to_hold_is_refused() {
    let refused = |set: &str| {
        let mistake = failure(&format!("SELECT {set} ON COLUMNS FROM [General Ledger]"));
        assert!(
            mistake.contains("a set has more than 1000000 tuples"),
            "{mistake}"
        );
    };
    // Nine periods, each giving the nine again, six times over: more than half a million.
    let mut periods = "[Period].Members".to_owned();
    for _ in 0..5 {
        periods = format!("Generate({periods}, [Period].Members, ALL)");
    }
    // Once more is nearly five million.
    refused(&format!("Generate({periods}, [Period].Members, ALL)"));
    // Sets multiplied together are refused before any of the product is built.
    refused(&format!("CrossJoin({periods}, [Entity].Members)"));
    // Unless only their tuples that hold something are wanted, which are found from the
    // cells without the product: here every period many times over, beside three entities.
    let mdx = format!(
        "SELECT NonEmptyCrossJoin({periods}, [Entity].Members) ON COLUMNS FROM [General Ledger] \
         WHERE ([Account].[Rent], [Version].[Budget])"
    );
    assert!(run(&mdx).axes[0].tuples.len() < 1_000_000);
}

#[test]
fn expressions_nested_too_deeply_are_refused() {
    let nested = |deep: usize| format!("{}1{}", "IIF(1 = 1, ".repeat(deep), ", 0)".repeat(deep));
    // As deep as is allowed is read and worked out.
    assert_eq!(number(&nested(30)), Some(1.0));
    for mdx in [
        nested(70),
        format!("{}1{}", "(".repeat(100_000), ")".repeat(100_000)),
        format!("{}1", "- ".repeat(100_000)),
        format!("{}1 = 1", "NOT ".repeat(100_000)),
        format!("{}1{}", "{".repeat(100_000), "}".repeat(100_000)),
    ] {
        let mistake = failure(&format!(
            "WITH MEMBER [Account].[X] AS {mdx} SELECT {{[Account].[X]}} ON COLUMNS FROM [General Ledger]"
        ));
        assert!(mistake.contains("nested more than 32 deep"), "{mistake}");
    }
}

#[test]
fn a_pattern_of_many_stars_is_matched_without_going_back_over_them() {
    let matching = |pattern: &str| {
        let mdx = format!(
            "SELECT TM1FilterByPattern([Account].Members, \"{pattern}\") ON COLUMNS FROM [General Ledger]"
        );
        names(&run(&mdx), 0)
    };
    assert_eq!(
        matching("*Revenue"),
        ["Revenue", "Product Revenue", "Service Revenue"]
    );
    assert_eq!(
        matching("*e*e*e"),
        ["Revenue", "Product Revenue", "Service Revenue"]
    );
    assert_eq!(matching("?ent"), ["Rent"]);
    assert_eq!(matching("Rent*"), ["Rent"]);
    // Letters match in either case.
    assert_eq!(
        matching("*R*t**"),
        ["Product Revenue", "Operating Expenses", "Rent"]
    );
    assert!(matching("Rent?").is_empty());
    // Forty stars that can never be satisfied: tried every way round, this would not end.
    assert!(matching(&format!("{}!", "*e".repeat(40))).is_empty());
}

/// A query whose calculated members each add the one before to itself, from a first one.
fn doubling(first: &str, members: usize) -> String {
    let mut mdx = format!("WITH MEMBER [Account].[M0] AS {first}");
    for member in 1..=members {
        let before = member - 1;
        mdx += &format!(
            " MEMBER [Account].[M{member}] AS [Account].[M{before}] + [Account].[M{before}]"
        );
    }
    mdx + &format!(" SELECT {{[Account].[M{members}]}} ON COLUMNS FROM [General Ledger]")
}

#[test]
fn text_that_doubles_is_refused_once_it_is_too_long() {
    assert_eq!(
        run(&doubling("('a' + 'b')", 3)).cells,
        [Some(Scalar::Text("ab".repeat(8)))]
    );
    assert_eq!(
        run(&doubling("1", 10)).cells,
        [Some(Scalar::Number(1024.0))]
    );
    let mistake = failure(&doubling("('a' + 'b')", 30));
    assert!(
        mistake.contains("a text has more than 1000000 characters"),
        "{mistake}"
    );
    // Ten letters, each made fifty of another, three times over.
    let mut grown = "('a' + 'aaaaaaaaa')".to_owned();
    for (from, to) in [('a', 'b'), ('b', 'c'), ('c', 'd')] {
        grown = format!(
            "Replace({grown}, '{from}', '{}')",
            to.to_string().repeat(50)
        );
    }
    let mistake = failure(&format!(
        "WITH MEMBER [Account].[X] AS {grown} SELECT {{[Account].[X]}} ON COLUMNS FROM [General Ledger]"
    ));
    assert!(
        mistake.contains("a text has more than 1000000 characters"),
        "{mistake}"
    );
}

/// A hundred million steps take a debug build ten seconds, and an optimised one three.
#[test]
#[cfg_attr(debug_assertions, ignore = "slow without optimisation")]
fn work_that_doubles_is_refused_once_it_is_too_much() {
    let mistake = failure(&doubling("1", 31));
    assert!(mistake.contains("more than 100000000 steps"), "{mistake}");
}

#[test]
fn a_flag_is_a_flag_even_beside_a_dimension_of_its_name() {
    // The example cube with one more dimension, called Desc.
    let mut definition = database().definition().clone();
    let desc =
        serde_json::json!({ "name": "Desc", "elements": [{ "name": "Only", "type": "numeric" }] });
    definition
        .dimensions
        .push(serde_json::from_value(desc).unwrap());
    definition.cubes[0].dimensions.push("Desc".to_owned());
    let mut named = Database::new(definition).unwrap();
    for (period, value) in [("Jan", 1.0), ("Feb", 3.0), ("Mar", 2.0)] {
        let write = CellWrite {
            cube: "General Ledger".to_owned(),
            at: ["Rent", "US", period, "Actual", "Only"]
                .map(str::to_owned)
                .to_vec(),
            value: Scalar::Number(value),
        };
        named.write(None, &[write]).unwrap();
    }
    let ordered = |flag: &str| {
        let mdx = format!(
            "WITH MEMBER [Version].[Rank] AS Rank([Period].CurrentMember, \
             Order({{[Period].[Jan], [Period].[Feb], [Period].[Mar]}}, [Version].[Actual], {flag})) \
             SELECT Order({{[Period].[Jan], [Period].[Feb], [Period].[Mar]}}, [Version].[Actual], {flag}) \
             ON COLUMNS FROM [General Ledger] WHERE ([Account].[Rent], [Entity].[US])"
        );
        names(&named.query(None, &mdx).unwrap(), 0)
    };
    assert_eq!(ordered("BDESC"), ["Feb", "Mar", "Jan"]);
    assert_eq!(ordered("DESC"), ["Feb", "Mar", "Jan"]);
    assert_eq!(ordered("ASC"), ["Jan", "Mar", "Feb"]);
    // The dimension is still reached by its name in full.
    let mdx = "SELECT [Desc].[Desc].Members ON COLUMNS FROM [General Ledger]";
    assert_eq!(names(&named.query(None, mdx).unwrap(), 0), ["Only"]);
}

#[test]
fn dimensions_are_taken_out_of_a_set() {
    let crossed = "{[Entity].[US], [Entity].[UK]} * {[Period].[Jan], [Period].[Feb]}";
    let columns = |set: &str| {
        let mdx = format!("SELECT {set} ON COLUMNS FROM [General Ledger]");
        names(&run(&mdx), 0)
    };
    // Each tuple of what is left appears once.
    assert_eq!(
        columns(&format!("Extract({crossed}, [Entity])")),
        ["US", "UK"]
    );
    assert_eq!(
        columns(&format!("Extract({crossed}, [Period])")),
        ["Jan", "Feb"]
    );
    let result = run(&format!(
        "SELECT Extract({crossed}, [Period], [Entity]) ON COLUMNS FROM [General Ledger]"
    ));
    assert_eq!(result.axes[0].tuples.len(), 4);
    assert_eq!(result.axes[0].tuples[1][0].name, "Feb");
    assert_eq!(result.axes[0].tuples[1][1].name, "US");
    for (arguments, expected) in [
        ("[Account]", "dimensions of the set"),
        ("[Entity], [Entity]", "each dimension once"),
        ("[Entity].[US]", "a set and then dimensions"),
    ] {
        let mistake = failure(&format!(
            "SELECT Extract({crossed}, {arguments}) ON COLUMNS FROM [General Ledger]"
        ));
        assert!(mistake.contains(expected), "{mistake}");
    }
}

/// The example model with its periods as a time dimension whose levels have names.
fn calendar() -> ModelDef {
    let mut definition = database().definition().clone();
    let period = &mut definition.dimensions[2];
    period.kind = epiphany_core::model::DimensionType::Time;
    period.levels = ["Month", "Quarter", "Half"].map(str::to_owned).to_vec();
    definition
}

#[test]
fn levels_are_known_by_the_names_the_model_gives_them() {
    let mut named = Database::new(calendar()).unwrap();
    named.write(None, &database().cells()).unwrap();
    let columns = |set: &str| {
        let mdx = format!("SELECT {set} ON COLUMNS FROM [General Ledger]");
        let result = named.query(None, &mdx);
        names(&result.unwrap_or_else(|error| panic!("{error}\n{mdx}")), 0)
    };
    let cell = |expression: &str| {
        let mdx = format!(
            "WITH MEMBER [Account].[X] AS {expression} SELECT {{[Account].[X]}} ON COLUMNS \
             FROM [General Ledger] WHERE ([Entity].[US], [Period].[Feb], [Version].[Actual])"
        );
        let result = named.query(None, &mdx);
        result
            .unwrap_or_else(|error| panic!("{error}\n{mdx}"))
            .cells[0]
            .clone()
    };
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun"];

    assert_eq!(columns("[Period].[Quarter].Members"), ["Q1", "Q2"]);
    assert_eq!(columns("[Period].Levels('half').Members"), ["H1"]);
    assert_eq!(columns("[Period].Levels(0).Members"), months);
    assert_eq!(
        cell("[Period].[Jan].Level.Name"),
        Some(Scalar::Text("Month".to_owned()))
    );
    assert_eq!(
        cell("[Period].Levels(1).Name"),
        Some(Scalar::Text("Quarter".to_owned()))
    );
    // A level without a name is known by its number.
    assert_eq!(
        cell("[Account].[Rent].Level.Name"),
        Some(Scalar::Text("0".to_owned()))
    );

    // Down to a level, whatever the distance.
    assert_eq!(
        columns("Descendants([Period].[H1], [Period].[Month])"),
        months
    );
    assert_eq!(
        columns("Descendants([Period].[H1], [Period].[Quarter])"),
        ["Q1", "Q2"]
    );
    assert_eq!(
        columns("Descendants([Period].[H1], [Period].[Quarter], AFTER)"),
        months
    );
    assert_eq!(
        columns("Descendants([Period].[Q2], [Period].[Quarter], SELF_AND_AFTER)"),
        ["Q2", "Apr", "May", "Jun"]
    );
    assert_eq!(
        columns("Descendants([Period].[H1], [Period].[Quarter], SELF_AND_BEFORE)"),
        ["H1", "Q1", "Q2"]
    );
    assert_eq!(
        columns("Descendants([Period].[Q1], 1)"),
        ["Jan", "Feb", "Mar"]
    );
    assert_eq!(columns("Ancestor([Period].[May], [Period].[Half])"), ["H1"]);
    assert_eq!(
        columns("ParallelPeriod([Period].[Quarter], 1, [Period].[May])"),
        ["Feb"]
    );

    // To date: within the quarter, the month, and at the current member of the time dimension.
    assert_eq!(columns("Qtd([Period].[May])"), ["Apr", "May"]);
    assert_eq!(columns("Qtd([Period].[Q2])"), ["Q2"]);
    assert_eq!(columns("Mtd([Period].[May])"), ["May"]);
    assert_eq!(
        cell("Sum(Qtd(), [Account].[Product Revenue])"),
        Some(Scalar::Number(120.0))
    );

    let failure = |mdx: &str| named.query(None, mdx).unwrap_err().to_string();
    let year = failure("SELECT Ytd([Period].[May]) ON COLUMNS FROM [General Ledger]");
    assert!(
        year.contains("YTD needs a level named 'Year' in dimension 'Period'"),
        "{year}"
    );
    let unknown =
        failure("SELECT [Period].Levels('Year').Members ON COLUMNS FROM [General Ledger]");
    assert!(
        unknown.contains("dimension 'Period' has no level 'Year'"),
        "{unknown}"
    );
    // In a cube with no time dimension the member must be given.
    let untimed = self::failure("SELECT Qtd() ON COLUMNS FROM [General Ledger]");
    assert!(
        untimed.contains("QTD needs a member, or a dimension of type 'time'"),
        "{untimed}"
    );
}

#[test]
fn a_level_cannot_share_its_name() {
    let invalid = |levels: [&str; 2]| {
        let mut definition = calendar();
        definition.dimensions[2].levels = levels.map(str::to_owned).to_vec();
        epiphany_core::model::Model::compile(&definition)
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        invalid(["Month", "month"]),
        "duplicate level 'month' in dimension 'Period'"
    );

    // With an element it can: the top of a calendar is as likely as not called Year. The
    // name then means the element, and the level is reached by Levels or by Ytd.
    let mut definition = calendar();
    definition.dimensions[2].levels = ["Month", "Quarter", "H1"].map(str::to_owned).to_vec();
    let shared = Database::new(definition).unwrap();
    let columns = |set: &str| {
        let mdx = format!("SELECT {set} ON COLUMNS FROM [General Ledger]");
        names(&shared.query(None, &mdx).unwrap(), 0)
    };
    assert_eq!(columns("{[Period].[H1]}"), ["H1"]);
    assert_eq!(columns("[Period].Levels('H1').Members"), ["H1"]);
    assert_eq!(columns("[Period].Levels('Quarter').Members"), ["Q1", "Q2"]);
}

#[test]
fn values_have_a_middle_and_a_spread() {
    // The days of the first three months, 31, 28 and 31, and of all six.
    let days = "[Period].CurrentMember.Properties('Days')";
    let quarter = "{[Period].[Jan], [Period].[Feb], [Period].[Mar]}";
    let of = |function: &str, set: &str| number(&format!("{function}({set}, {days})"));
    assert_eq!(of("Median", quarter), Some(31.0));
    assert_eq!(of("Median", "[Period].Levels(0).Members"), Some(30.5));
    assert_eq!(of("Var", quarter), Some(3.0));
    assert_eq!(of("VarP", quarter), Some(2.0));
    assert_eq!(of("Stdev", quarter), Some(3f64.sqrt()));
    assert_eq!(of("StdevP", quarter), Some(2f64.sqrt()));
    assert_eq!(of("StdDev", quarter), of("Stdev", quarter));
    assert_eq!(of("Variance", quarter), of("Var", quarter));
    // One value alone has no spread as a sample, and none at all as a population.
    assert_eq!(of("Stdev", "{[Period].[Jan]}"), None);
    assert_eq!(of("StdevP", "{[Period].[Jan]}"), Some(0.0));
    // Over cells, the empty ones are left out, here March.
    let revenue = "([Account].[Product Revenue], [Entity].[US])";
    assert_eq!(number(&format!("Median({quarter}, {revenue})")), Some(60.0));
    assert_eq!(
        number(&format!("Median({{[Period].[Mar]}}, {revenue})")),
        None
    );
}

#[test]
fn the_top_of_a_set_is_taken_by_sum_or_by_share() {
    // January's product revenue: 150 in all, 100 in the US and 50 in the UK.
    let by = "([Account].[Product Revenue], [Period].[Jan], [Version].[Actual])";
    let columns = |function: &str, amount: f64| {
        let mdx = format!(
            "SELECT {function}([Entity].Members, {amount}, {by}) ON COLUMNS FROM [General Ledger]"
        );
        names(&run(&mdx), 0)
    };
    // As many, from the greatest down, as it takes to reach the sum.
    assert_eq!(columns("TopSum", 200.0), ["Total Entity", "US"]);
    assert_eq!(columns("TopSum", 150.0), ["Total Entity"]);
    assert_eq!(columns("TopSum", 1000.0), ["Total Entity", "US", "UK"]);
    assert_eq!(columns("TopSum", 0.0), [""; 0]);
    assert_eq!(columns("BottomSum", 60.0), ["UK", "US"]);
    // Or to reach a share of the whole, which here is 300.
    assert_eq!(columns("TopPercent", 50.0), ["Total Entity"]);
    assert_eq!(columns("TopPercent", 51.0), ["Total Entity", "US"]);
    assert_eq!(columns("BottomPercent", 10.0), ["UK"]);
    let mistake = failure("SELECT TopSum([Entity].Members, 5) ON COLUMNS FROM [General Ledger]");
    assert!(
        mistake.contains("TOPSUM takes 3 to 3 arguments"),
        "{mistake}"
    );
}

#[test]
fn a_level_means_nothing_to_another_dimension() {
    for set in [
        "Descendants([Account].[Net Income], [Period].Levels(1))",
        "{Ancestor([Account].[Rent], [Period].Levels(1))}",
        "PeriodsToDate([Period].Levels(2), [Account].[Rent])",
        "{ParallelPeriod([Period].Levels(1), 1, [Account].[Rent])}",
    ] {
        let mistake = failure(&format!("SELECT {set} ON COLUMNS FROM [General Ledger]"));
        let expected =
            "the level is one of dimension 'Period', and the member belongs to 'Account'";
        assert!(mistake.contains(expected), "{set}: {mistake}");
    }
}

#[test]
fn a_share_of_a_loss_is_reached_from_above() {
    // January's product revenue taken as a loss: -150 in all, -100 in the US, -50 in the UK.
    let by = "-([Account].[Product Revenue], [Period].[Jan], [Version].[Actual])";
    let columns = |function: &str, amount: f64| {
        let mdx = format!(
            "SELECT {function}([Entity].Members, {amount}, {by}) ON COLUMNS FROM [General Ledger]"
        );
        names(&run(&mdx), 0)
    };
    // Half of the whole, which is -300, is -150.
    assert_eq!(columns("TopPercent", 50.0), ["UK", "US"]);
    assert_eq!(columns("BottomPercent", 50.0), ["Total Entity"]);
    assert_eq!(columns("BottomSum", -200.0), ["Total Entity", "US"]);
}
