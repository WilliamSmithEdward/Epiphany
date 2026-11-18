use epiphany_core::Value;
use epiphany_core::cells::CellError;
use epiphany_core::database::{CellWrite, Database, Error};
use epiphany_core::mdx::{MemberType, QueryResult};
use epiphany_core::model::ModelDef;

const CUBE: &str = "General Ledger";

fn set(at: [&str; 4], value: f64) -> CellWrite {
    CellWrite {
        cube: CUBE.to_owned(),
        at: at.map(str::to_owned).to_vec(),
        value: Value::Number(value),
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
    let number = |cell: &Option<Value>| match cell {
        Some(Value::Number(number)) => Some(*number),
        Some(Value::Text(text)) => panic!("expected a number, found '{text}'"),
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
        Value::Text("6100".to_owned())
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
    let write = |at: [&str; 2], value: Value| CellWrite {
        cube: "Stock".to_owned(),
        at: at.map(str::to_owned).to_vec(),
        value,
    };
    let writes = [
        write(["A", "Units"], Value::Number(5.0)),
        write(["A", "Note"], Value::Text("late".to_owned())),
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
    let text = |text: &str| Some(Value::Text(text.to_owned()));
    assert_eq!(
        result.cells,
        [
            Some(Value::Number(5.0)),
            text("late"),
            text("A: late"),
            Some(Value::Number(0.0)),
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
