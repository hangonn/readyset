use std::{fmt, str};

use itertools::Itertools;
use nom::branch::alt;
use nom::bytes::complete::tag_no_case;
use nom::combinator::{map, opt};
use nom::multi::separated_list1;
use nom::sequence::{terminated, tuple};
use nom::{IResult, Parser};
use serde::{Deserialize, Serialize};

use crate::common::statement_terminator;
use crate::expression::expression;
use crate::whitespace::{whitespace0, whitespace1};
use crate::{Dialect, Expression, SqlIdentifier};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum SetStatement {
    Variable(SetVariables),
    Names(SetNames),
}

impl fmt::Display for SetStatement {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SET ")?;
        match self {
            Self::Variable(set) => write!(f, "{}", set)?,
            Self::Names(set) => write!(f, "{}", set)?,
        };
        Ok(())
    }
}

impl SetStatement {
    pub fn variables(&self) -> Option<&[(Variable, Expression)]> {
        match self {
            SetStatement::Names(_) => None,
            SetStatement::Variable(set) => Some(&set.variables),
        }
    }
}

/// Scope for a [`Variable`]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum VariableScope {
    User,
    Local,
    Global,
    Session,
}

impl fmt::Display for VariableScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VariableScope::User => Ok(()),
            VariableScope::Local => write!(f, "LOCAL"),
            VariableScope::Global => write!(f, "GLOBAL"),
            VariableScope::Session => write!(f, "SESSION"),
        }
    }
}

pub(crate) fn variable_scope_prefix(i: &[u8]) -> IResult<&[u8], VariableScope> {
    alt((
        map(tag_no_case("@@LOCAL."), |_| VariableScope::Local),
        map(tag_no_case("@@GLOBAL."), |_| VariableScope::Global),
        map(tag_no_case("@@SESSION."), |_| VariableScope::Session),
        map(tag_no_case("@@"), |_| VariableScope::Session),
        map(tag_no_case("@"), |_| VariableScope::User),
    ))(i)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Variable {
    pub scope: VariableScope,
    pub name: SqlIdentifier,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct SetVariables {
    /// A list of variables and their assigned values
    pub variables: Vec<(Variable, Expression)>,
}

impl Variable {
    /// If the variable is one of Local, Global or Session, returns the variable name
    pub fn as_non_user_var(&self) -> Option<&str> {
        if self.scope == VariableScope::User {
            None
        } else {
            Some(&self.name)
        }
    }
}

impl fmt::Display for Variable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.scope == VariableScope::User {
            write!(f, "@")?;
        } else {
            write!(f, "@@{}.", self.scope)?;
        }
        write!(f, "{}", self.name)
    }
}

impl fmt::Display for SetVariables {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            self.variables
                .iter()
                .map(|(var, value)| format!("{} = {}", var, value))
                .join(", ")
        )
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct SetNames {
    pub charset: String,
    pub collation: Option<String>,
}

impl fmt::Display for SetNames {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "NAMES '{}'", &self.charset)?;
        if let Some(collation) = self.collation.as_ref() {
            write!(f, " COLLATE '{}'", collation)?;
        }
        Ok(())
    }
}

fn set_variable_scope_prefix(i: &[u8]) -> IResult<&[u8], VariableScope> {
    alt((
        variable_scope_prefix,
        map(terminated(tag_no_case("GLOBAL"), whitespace1), |_| {
            VariableScope::Global
        }),
        map(terminated(tag_no_case("SESSION"), whitespace1), |_| {
            VariableScope::Session
        }),
        map(terminated(tag_no_case("LOCAL"), whitespace1), |_| {
            VariableScope::Local
        }),
    ))(i)
}

/// check for one of three ways to specify scope and reformat to a single formatting. Returns none
/// if scope is not specified
fn variable(dialect: Dialect) -> impl Fn(&[u8]) -> IResult<&[u8], Variable> {
    move |i| {
        let (i, scope) = set_variable_scope_prefix
            .or(|i| Ok((i, VariableScope::Local)))
            .parse(i)?;
        let (i, name) = dialect
            .identifier()
            .map(|ident| ident.to_ascii_lowercase().into())
            .parse(i)?;
        Ok((i, Variable { scope, name }))
    }
}

pub fn set(dialect: Dialect) -> impl Fn(&[u8]) -> IResult<&[u8], SetStatement> {
    move |i| {
        let (i, _) = tag_no_case("set")(i)?;
        let (i, _) = whitespace1(i)?;
        let (i, statement) = alt((
            map(set_variables(dialect), SetStatement::Variable),
            map(set_names(dialect), SetStatement::Names),
        ))(i)?;

        Ok((i, statement))
    }
}

fn set_variable(dialect: Dialect) -> impl Fn(&[u8]) -> IResult<&[u8], (Variable, Expression)> {
    move |i| {
        let (i, variable) = variable(dialect)(i)?;
        let (i, _) = whitespace0(i)?;
        let (i, _) = alt((tag_no_case("="), tag_no_case(":=")))(i)?;
        let (i, _) = whitespace0(i)?;
        let (i, value) = expression(dialect)(i)?;
        Ok((i, (variable, value)))
    }
}

fn set_variables(dialect: Dialect) -> impl Fn(&[u8]) -> IResult<&[u8], SetVariables> {
    move |i| {
        let (remaining_input, variables) = terminated(
            separated_list1(
                tuple((tag_no_case(","), whitespace0)),
                set_variable(dialect),
            ),
            statement_terminator,
        )(i)?;

        Ok((remaining_input, SetVariables { variables }))
    }
}

fn set_names(dialect: Dialect) -> impl Fn(&[u8]) -> IResult<&[u8], SetNames> {
    move |i| {
        let (i, _) = tag_no_case("names")(i)?;
        let (i, _) = whitespace1(i)?;
        let (i, charset) = dialect.utf8_string_literal()(i)?;
        let (i, collation) = opt(move |i| {
            let (i, _) = whitespace1(i)?;
            let (i, _) = tag_no_case("collate")(i)?;
            let (i, _) = whitespace1(i)?;
            let (i, collation) = dialect.utf8_string_literal()(i)?;
            Ok((i, collation))
        })(i)?;

        Ok((i, SetNames { charset, collation }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_set() {
        let qstring = "SET SQL_AUTO_IS_NULL = 0;";
        let res = set(Dialect::MySQL)(qstring.as_bytes());
        assert_eq!(
            res.unwrap().1,
            SetStatement::Variable(SetVariables {
                variables: vec!((
                    Variable {
                        scope: VariableScope::Local,
                        name: "sql_auto_is_null".into()
                    },
                    Expression::Literal(0.into())
                )),
            })
        );
    }

    #[test]
    fn user_defined_vars() {
        let qstring = "SET @var = 123;";
        let res = set(Dialect::MySQL)(qstring.as_bytes());
        assert_eq!(
            res.unwrap().1,
            SetStatement::Variable(SetVariables {
                variables: vec!((
                    Variable {
                        scope: VariableScope::User,
                        name: "var".into()
                    },
                    Expression::Literal(123.into())
                )),
            })
        );
    }

    #[test]
    fn format_set() {
        let qstring = "set autocommit=1";
        let expected = "SET @@LOCAL.autocommit = 1";
        let res = set(Dialect::MySQL)(qstring.as_bytes());
        assert_eq!(format!("{}", res.unwrap().1), expected);
    }

    #[test]
    fn global_set() {
        let qstring1 = "set gloBal var = 2";
        let qstring2 = "set @@gLobal.var = 2";
        let expected = "SET @@GLOBAL.var = 2";
        let res1 = test_parse!(set(Dialect::MySQL), qstring1.as_bytes());
        let res2 = test_parse!(set(Dialect::MySQL), qstring2.as_bytes());
        assert_eq!(format!("{}", res1), expected);
        assert_eq!(format!("{}", res2), expected);
    }

    #[test]
    fn session_set() {
        let qstring1 = "set @@Session.var = 1";
        let qstring2 = "set @@var = 1";
        let qstring3 = "set SeSsion var = 1";
        let expected = "SET @@SESSION.var = 1";
        let res1 = set(Dialect::MySQL)(qstring1.as_bytes());
        let res2 = set(Dialect::MySQL)(qstring2.as_bytes());
        let res3 = set(Dialect::MySQL)(qstring3.as_bytes());
        assert_eq!(format!("{}", res1.unwrap().1), expected);
        assert_eq!(format!("{}", res2.unwrap().1), expected);
        assert_eq!(format!("{}", res3.unwrap().1), expected);
    }

    #[test]
    fn local_set() {
        let qstring1 = "set lOcal var = 2";
        let qstring2 = "set @@local.var = 2";
        let expected = "SET @@LOCAL.var = 2";
        let res1 = set(Dialect::MySQL)(qstring1.as_bytes());
        let res2 = set(Dialect::MySQL)(qstring2.as_bytes());
        assert_eq!(format!("{}", res1.unwrap().1), expected);
        assert_eq!(format!("{}", res2.unwrap().1), expected);
    }

    #[test]
    fn set_names() {
        let qstring1 = "SET NAMES 'iso8660'";
        let qstring2 = "set names 'utf8mb4' collate 'utf8mb4_unicode_ci'";
        let res1 = set(Dialect::MySQL)(qstring1.as_bytes()).unwrap().1;
        let res2 = set(Dialect::MySQL)(qstring2.as_bytes()).unwrap().1;
        assert_eq!(
            res1,
            SetStatement::Names(SetNames {
                charset: "iso8660".to_string(),
                collation: None
            })
        );
        assert_eq!(
            res2,
            SetStatement::Names(SetNames {
                charset: "utf8mb4".to_string(),
                collation: Some("utf8mb4_unicode_ci".to_string())
            })
        );
    }

    #[test]
    fn expression_set() {
        let qstring = "SET @myvar = 100 + 200;";
        let res = set(Dialect::MySQL)(qstring.as_bytes());
        assert_eq!(
            res.unwrap().1,
            SetStatement::Variable(SetVariables {
                variables: vec!((
                    Variable {
                        scope: VariableScope::User,
                        name: "myvar".into()
                    },
                    Expression::BinaryOp {
                        lhs: Box::new(Expression::Literal(100.into())),
                        op: crate::BinaryOperator::Add,
                        rhs: Box::new(Expression::Literal(200.into())),
                    }
                )),
            })
        );
    }

    #[test]
    fn list_set() {
        let qstring = "SET @myvar = 100 + 200, @@notmyvar = 'value', @@Global.g = @@global.V;";
        let res = set(Dialect::MySQL)(qstring.as_bytes());
        assert_eq!(
            res.unwrap().1,
            SetStatement::Variable(SetVariables {
                variables: vec!(
                    (
                        Variable {
                            scope: VariableScope::User,
                            name: "myvar".into()
                        },
                        Expression::BinaryOp {
                            lhs: Box::new(Expression::Literal(100.into())),
                            op: crate::BinaryOperator::Add,
                            rhs: Box::new(Expression::Literal(200.into())),
                        }
                    ),
                    (
                        Variable {
                            scope: VariableScope::Session,
                            name: "notmyvar".into()
                        },
                        Expression::Literal("value".into()),
                    ),
                    (
                        Variable {
                            scope: VariableScope::Global,
                            name: "g".into()
                        },
                        Expression::Variable(Variable {
                            scope: VariableScope::Global,
                            name: "v".into()
                        }),
                    )
                ),
            })
        );
    }
}
