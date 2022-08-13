// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkVM library.

// The snarkVM library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkVM library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkVM library. If not, see <https://www.gnu.org/licenses/>.

mod authorization;
pub use authorization::*;

mod deployment;
pub use deployment::*;

mod execution;
pub use execution::*;

mod register_types;
pub use register_types::*;

mod registers;
pub use registers::*;

mod authorize;
mod deploy;
mod evaluate;
mod execute;
mod helpers;

use crate::{
    CallOperator,
    Certificate,
    Closure,
    Function,
    Instruction,
    Opcode,
    Operand,
    Process,
    Program,
    ProvingKey,
    Transition,
    UniversalSRS,
    VerifyingKey,
};
use console::{
    account::{Address, PrivateKey},
    network::prelude::*,
    program::{
        Balance,
        Entry,
        EntryType,
        Identifier,
        Literal,
        Locator,
        Owner,
        Plaintext,
        PlaintextType,
        ProgramID,
        Record,
        RecordType,
        Register,
        RegisterType,
        Request,
        Response,
        Value,
        ValueType,
    },
    types::{Field, Group, U64},
};

use indexmap::IndexMap;
use parking_lot::RwLock;
use std::sync::Arc;

pub type Assignments<N> = Arc<RwLock<Vec<circuit::Assignment<<N as Environment>::Field>>>>;

#[derive(Clone)]
pub enum CallStack<N: Network> {
    Authorize(Vec<Request<N>>, PrivateKey<N>, Authorization<N>),
    Synthesize(Vec<Request<N>>, PrivateKey<N>, Authorization<N>),
    CheckDeployment(Vec<Request<N>>, PrivateKey<N>, Assignments<N>),
    Evaluate(Authorization<N>),
    Execute(Authorization<N>, Arc<RwLock<Execution<N>>>),
}

impl<N: Network> CallStack<N> {
    /// Initializes a call stack as `Evaluate`.
    pub fn evaluate(authorization: Authorization<N>) -> Result<Self> {
        Ok(CallStack::Evaluate(authorization))
    }

    /// Initializes a call stack as `Execute`.
    pub fn execute(authorization: Authorization<N>, execution: Arc<RwLock<Execution<N>>>) -> Result<Self> {
        Ok(CallStack::Execute(authorization, execution))
    }
}

impl<N: Network> CallStack<N> {
    /// Returns a new and independent replica of the call stack.
    pub fn replicate(&self) -> Self {
        match self {
            CallStack::Authorize(requests, private_key, authorization) => {
                CallStack::Authorize(requests.clone(), *private_key, authorization.replicate())
            }
            CallStack::Synthesize(requests, private_key, authorization) => {
                CallStack::Synthesize(requests.clone(), *private_key, authorization.replicate())
            }
            CallStack::CheckDeployment(requests, private_key, assignments) => CallStack::CheckDeployment(
                requests.clone(),
                *private_key,
                Arc::new(RwLock::new(assignments.read().clone())),
            ),
            CallStack::Evaluate(authorization) => CallStack::Evaluate(authorization.replicate()),
            CallStack::Execute(authorization, execution) => {
                CallStack::Execute(authorization.replicate(), Arc::new(RwLock::new(execution.read().clone())))
            }
        }
    }

    /// Pushes the request to the stack.
    pub fn push(&mut self, request: Request<N>) -> Result<()> {
        match self {
            CallStack::Authorize(requests, ..) => requests.push(request),
            CallStack::Synthesize(requests, ..) => requests.push(request),
            CallStack::CheckDeployment(requests, ..) => requests.push(request),
            CallStack::Evaluate(authorization) => authorization.push(request),
            CallStack::Execute(authorization, ..) => authorization.push(request),
        }
        Ok(())
    }

    /// Pops the request from the stack.
    pub fn pop(&mut self) -> Result<Request<N>> {
        match self {
            CallStack::Authorize(requests, ..)
            | CallStack::Synthesize(requests, ..)
            | CallStack::CheckDeployment(requests, ..) => {
                requests.pop().ok_or_else(|| anyhow!("No more requests on the stack"))
            }
            CallStack::Evaluate(authorization) => authorization.next(),
            CallStack::Execute(authorization, ..) => authorization.next(),
        }
    }

    /// Peeks at the next request from the stack.
    pub fn peek(&mut self) -> Result<Request<N>> {
        match self {
            CallStack::Authorize(requests, ..)
            | CallStack::Synthesize(requests, ..)
            | CallStack::CheckDeployment(requests, ..) => {
                requests.last().cloned().ok_or_else(|| anyhow!("No more requests on the stack"))
            }
            CallStack::Evaluate(authorization) => authorization.peek_next(),
            CallStack::Execute(authorization, ..) => authorization.peek_next(),
        }
    }
}

#[derive(Clone)]
pub struct Stack<N: Network> {
    /// The program (record types, interfaces, functions).
    program: Program<N>,
    /// The mapping of external stacks as `(program ID, stack)`.
    external_stacks: IndexMap<ProgramID<N>, Stack<N>>,
    /// The mapping of closure and function names to their register types.
    program_types: IndexMap<Identifier<N>, RegisterTypes<N>>,
    /// The universal SRS.
    universal_srs: Arc<UniversalSRS<N>>,
    /// The mapping of function name to proving key.
    proving_keys: Arc<RwLock<IndexMap<Identifier<N>, ProvingKey<N>>>>,
    /// The mapping of function name to verifying key.
    verifying_keys: Arc<RwLock<IndexMap<Identifier<N>, VerifyingKey<N>>>>,
}

impl<N: Network> Stack<N> {
    /// Initializes a new stack, if it does not already exist, given the process and the program.
    #[inline]
    pub fn new(process: &Process<N>, program: &Program<N>) -> Result<Self> {
        // Retrieve the program ID.
        let program_id = program.id();
        // Ensure the program does not already exist in the process.
        ensure!(!process.contains_program(program_id), "Program '{program_id}' already exists");
        // Ensure the program network-level domain (NLD) is correct.
        ensure!(program_id.is_aleo(), "Program '{program_id}' has an incorrect network-level domain (NLD)");
        // Ensure the program contains functions.
        ensure!(!program.functions().is_empty(), "No functions present in the deployment for program '{program_id}'");

        // Serialize the program into bytes.
        let program_bytes = program.to_bytes_le()?;
        // Ensure the program deserializes from bytes correctly.
        ensure!(program == &Program::from_bytes_le(&program_bytes)?, "Program byte serialization failed");

        // Serialize the program into string.
        let program_string = program.to_string();
        // Ensure the program deserializes from a string correctly.
        ensure!(program == &Program::from_str(&program_string)?, "Program string serialization failed");

        // Return the stack.
        Stack::initialize(process, program)
    }

    /// Returns the program.
    #[inline]
    pub const fn program(&self) -> &Program<N> {
        &self.program
    }

    /// Returns the program ID.
    #[inline]
    pub const fn program_id(&self) -> &ProgramID<N> {
        self.program.id()
    }

    /// Returns `true` if the stack contains the external record.
    #[inline]
    pub fn contains_external_record(&self, locator: &Locator<N>) -> bool {
        // Retrieve the external program.
        match self.get_external_program(locator.program_id()) {
            // Return `true` if the external record exists.
            Ok(external_program) => external_program.contains_record(locator.resource()),
            // Return `false` otherwise.
            Err(_) => false,
        }
    }

    /// Returns the external stack for the given program ID.
    #[inline]
    pub fn get_external_stack(&self, program_id: &ProgramID<N>) -> Result<&Stack<N>> {
        // Retrieve the external stack.
        self.external_stacks.get(program_id).ok_or_else(|| anyhow!("External program '{program_id}' does not exist."))
    }

    /// Returns the external program for the given program ID.
    #[inline]
    pub fn get_external_program(&self, program_id: &ProgramID<N>) -> Result<&Program<N>> {
        match self.program.id() == program_id {
            true => bail!("Attempted to get the main program '{}' as an external program", self.program.id()),
            // Retrieve the external stack, and return the external program.
            false => Ok(self.get_external_stack(program_id)?.program()),
        }
    }

    /// Returns `true` if the stack contains the external record.
    #[inline]
    pub fn get_external_record(&self, locator: &Locator<N>) -> Result<RecordType<N>> {
        // Retrieve the external program.
        let external_program = self.get_external_program(locator.program_id())?;
        // Return the external record, if it exists.
        external_program.get_record(locator.resource())
    }

    /// Returns the function with the given function name.
    #[inline]
    pub fn get_function(&self, function_name: &Identifier<N>) -> Result<Function<N>> {
        // Ensure the function exists.
        match self.program.contains_function(function_name) {
            true => self.program.get_function(function_name),
            false => bail!("Function '{function_name}' does not exist in program '{}'.", self.program.id()),
        }
    }

    /// Returns the expected number of calls for the given function name.
    #[inline]
    pub fn get_number_of_calls(&self, function_name: &Identifier<N>) -> Result<usize> {
        // Determine the number of calls for this function (including the function itself).
        let mut num_calls = 1;
        for instruction in self.get_function(function_name)?.instructions() {
            if let Instruction::Call(call) = instruction {
                // Determine if this is a function call.
                if call.is_function_call(self)? {
                    // Increment by the number of calls.
                    num_calls += match call.operator() {
                        CallOperator::Locator(locator) => {
                            self.get_external_stack(locator.program_id())?.get_number_of_calls(locator.resource())?
                        }
                        CallOperator::Resource(resource) => self.get_number_of_calls(resource)?,
                    };
                }
            }
        }
        Ok(num_calls)
    }

    /// Returns the register types for the given closure or function name.
    #[inline]
    pub fn get_register_types(&self, name: &Identifier<N>) -> Result<&RegisterTypes<N>> {
        // Retrieve the register types.
        self.program_types.get(name).ok_or_else(|| anyhow!("Register types for '{name}' does not exist"))
    }

    /// Returns `true` if the proving key for the given function name exists.
    #[inline]
    pub fn contains_proving_key(&self, function_name: &Identifier<N>) -> bool {
        self.proving_keys.read().contains_key(function_name)
    }

    /// Returns `true` if the verifying key for the given function name exists.
    #[inline]
    pub fn contains_verifying_key(&self, function_name: &Identifier<N>) -> bool {
        self.verifying_keys.read().contains_key(function_name)
    }

    /// Returns the proving key for the given function name.
    #[inline]
    pub fn get_proving_key(&self, function_name: &Identifier<N>) -> Result<ProvingKey<N>> {
        // Return the proving key, if it exists.
        match self.proving_keys.read().get(function_name) {
            Some(proving_key) => Ok(proving_key.clone()),
            None => bail!("Proving key not found for: {}/{function_name}", self.program.id()),
        }
    }

    /// Returns the verifying key for the given function name.
    #[inline]
    pub fn get_verifying_key(&self, function_name: &Identifier<N>) -> Result<VerifyingKey<N>> {
        // Return the verifying key, if it exists.
        match self.verifying_keys.read().get(function_name) {
            Some(verifying_key) => Ok(verifying_key.clone()),
            None => bail!("Verifying key not found for: {}/{function_name}", self.program.id()),
        }
    }

    /// Inserts the given proving key for the given function name.
    #[inline]
    pub fn insert_proving_key(&self, function_name: &Identifier<N>, proving_key: ProvingKey<N>) -> Result<()> {
        // Ensure the function name exists in the program.
        ensure!(
            self.program.contains_function(function_name),
            "Function '{function_name}' does not exist in program '{}'.",
            self.program.id()
        );
        // Insert the proving key.
        self.proving_keys.write().insert(*function_name, proving_key);
        Ok(())
    }

    /// Inserts the given verifying key for the given function name.
    #[inline]
    pub fn insert_verifying_key(&self, function_name: &Identifier<N>, verifying_key: VerifyingKey<N>) -> Result<()> {
        // Ensure the function name exists in the program.
        ensure!(
            self.program.contains_function(function_name),
            "Function '{function_name}' does not exist in program '{}'.",
            self.program.id()
        );
        // Insert the verifying key.
        self.verifying_keys.write().insert(*function_name, verifying_key);
        Ok(())
    }

    /// Removes the proving key for the given function name.
    #[inline]
    pub fn remove_proving_key(&self, function_name: &Identifier<N>) {
        self.proving_keys.write().remove(function_name);
    }

    /// Removes the verifying key for the given function name.
    #[inline]
    pub fn remove_verifying_key(&self, function_name: &Identifier<N>) {
        self.verifying_keys.write().remove(function_name);
    }
}

impl<N: Network> PartialEq for Stack<N> {
    fn eq(&self, other: &Self) -> bool {
        self.program == other.program
            && self.external_stacks == other.external_stacks
            && self.program_types == other.program_types
    }
}

impl<N: Network> Eq for Stack<N> {}
