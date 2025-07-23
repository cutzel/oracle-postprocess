use serde_derive::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum DecompileOptions {
    V1(V1DecompileOptions),
    V2(V2DecompileOptions),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum V1RenamingType {
    #[serde(rename = "NONE")]
    None,
    #[serde(rename = "UNIQUE")]
    Unique,
    #[serde(rename = "UNIQUE_VALUE_BASED")]
    UniqueValueBased,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct V1DecompileOptions {
    #[serde(rename = "renamingType")]
    renaming_type: Option<V1RenamingType>,
    #[serde(rename = "removeDotZero")]
    remove_dot_zero: Option<bool>,
    #[serde(rename = "removeFunctionEntryNote")]
    remove_function_entry_note: Option<bool>,
    #[serde(rename = "swapConstantPosition")]
    swap_constant_position: Option<bool>,
    #[serde(rename = "inlineWhileConditions")]
    inline_while_conditions: Option<bool>,
    #[serde(rename = "showFunctionLineDefined")]
    show_function_line_defined: Option<bool>,
    #[serde(rename = "removeUselessNumericForStep")]
    remove_useless_numeric_for_step: Option<bool>,
    #[serde(rename = "removeUselessReturnInFunction")]
    remove_useless_return_in_function: Option<bool>,
    #[serde(rename = "sugarRecursiveLocalFunctions")]
    sugar_recursive_local_functions: Option<bool>,
    #[serde(rename = "sugarLocalFunctions")]
    sugar_local_functions: Option<bool>,
    #[serde(rename = "sugarGlobalFunctions")]
    sugar_global_functions: Option<bool>,
    #[serde(rename = "sugarGenericFor")]
    sugar_generic_for: Option<bool>,
    #[serde(rename = "showFunctionDebugName")]
    show_function_debug_name: Option<bool>,
    #[serde(rename = "sugarRepeatLoops")]
    sugar_repeat_loops: Option<bool>,
    #[serde(rename = "upvalueComment")]
    upvalue_comment: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct V2DecompileOptions {
    // we have no options for v2 yet
}
