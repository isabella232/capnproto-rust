/*
 * Copyright (c) 2014, David Renshaw (dwrenshaw@gmail.com)
 *
 * See the LICENSE file in the capnproto-rust root directory.
 */

use capnp::{AnyPointer};
use capnp::capability;
use capnp::capability::{CallContextHook, ClientHook, PipelineHook, PipelineOp, ResultFuture,
                        RequestHook, Request, ResponseHook};
use capnp::common;
use capnp::{ReaderOptions, MessageReader, BuilderOptions, MessageBuilder, MallocMessageBuilder};
use capnp::serialize;
use capnp::OwnedSpaceMessageReader;

use std;
use std::any::AnyRefExt;
use std::vec::Vec;
use collections::hashmap::HashMap;
use collections::priority_queue::PriorityQueue;

use rpc_capnp::{Message, Return, CapDescriptor, MessageTarget, Payload, PromisedAnswer};

pub type QuestionId = u32;
pub type AnswerId = QuestionId;
pub type ExportId = u32;
pub type ImportId = ExportId;

pub struct Question {
    chan : std::comm::Sender<~ResponseHook:Send>,
    is_awaiting_return : bool,
}

pub enum AnswerStatus {
    AnswerStatusSent(~MallocMessageBuilder),
    AnswerStatusPending(Vec<(u64, u16, Vec<PipelineOp::Type>, ~CallContextHook:Send)>),
}

pub struct Answer {
    status : AnswerStatus,
    result_exports : Vec<ExportId>,
    pipeline : Option<~PipelineHook:Send>,
}

impl Answer {
    pub fn new() -> Answer {
        Answer {
            status : AnswerStatusPending(Vec::new()),
            result_exports : Vec::new(),
            pipeline : None,
        }
    }

    fn do_call(answer_message : &mut ~MallocMessageBuilder, interface_id : u64, method_id : u16,
               ops : Vec<PipelineOp::Type>, context : ~CallContextHook:Send) {
        let root : Message::Builder = answer_message.get_root();
        match root.which() {
            Some(Message::Return(ret)) => {
                match ret.which() {
                    Some(Return::Results(payload)) => {
                        let hook = payload.get_content().as_reader().
                            get_pipelined_cap(ops.as_slice());
                        hook.call(interface_id, method_id, context);
                    }
                    Some(Return::Exception(_exc)) => {
                        // TODO
                    }
                    _ => fail!(),
                }
            }
            _ => fail!(),
        }
    }

    pub fn receive(&mut self, interface_id : u64, method_id : u16,
                   ops : Vec<PipelineOp::Type>, context : ~CallContextHook:Send) {
        match self.status {
            AnswerStatusSent(ref mut answer_message) => {
                Answer::do_call(answer_message, interface_id, method_id, ops, context);
            }
            AnswerStatusPending(ref mut waiters) => {
                waiters.push((interface_id, method_id, ops, context));
            }
        }
    }

    pub fn sent(&mut self, mut message : ~MallocMessageBuilder) {
        match self.status {
            AnswerStatusSent(_) => {fail!()}
            AnswerStatusPending(ref mut waiters) => {
                waiters.reverse();
                while waiters.len() > 0 {
                    let (interface_id, method_id, ops, context) = match waiters.pop() {
                        Some(r) => r,
                        None => fail!(),
                    };
                    Answer::do_call(&mut message, interface_id, method_id, ops, context);
                }
            }
        }
        self.status = AnswerStatusSent(message);
    }
}

pub struct Export {
    hook : ~ClientHook:Send,
}

pub struct Import;

pub struct ImportTable<T> {
    slots : HashMap<u32, T>,
}

impl <T> ImportTable<T> {
    pub fn new() -> ImportTable<T> {
        ImportTable { slots : HashMap::new() }
    }
}

#[deriving(Eq)]
struct ReverseU32 { val : u32 }

impl ::std::cmp::Ord for ReverseU32 {
    fn lt(&self, other : &ReverseU32) -> bool {
        self.val > other.val
    }
}

pub struct ExportTable<T> {
    slots : Vec<Option<T>>,

    // prioritize lower values
    free_ids : PriorityQueue<ReverseU32>,
}

impl <T> ExportTable<T> {
    pub fn new() -> ExportTable<T> {
        ExportTable { slots : Vec::new(),
                      free_ids : PriorityQueue::new() }
    }

    pub fn erase(&mut self, id : u32) {
        *self.slots.get_mut(id as uint) = None;
        self.free_ids.push(ReverseU32 { val : id } );
    }

    pub fn push(&mut self, val : T) -> u32 {
        match self.free_ids.maybe_pop() {
            Some(ReverseU32 { val : id }) => {
                *self.slots.get_mut(id as uint) = Some(val);
                id
            }
            None => {
                self.slots.push(Some(val));
                self.slots.len() as u32 - 1
            }
        }
    }
}

pub trait SturdyRefRestorer {
    fn restore(&self, _obj_id : AnyPointer::Reader) -> Option<~ClientHook:Send> { None }
}

impl SturdyRefRestorer for () { }


pub struct RpcConnectionState {
    exports : ExportTable<Export>,
    questions : ExportTable<Question>,
    answers : ImportTable<Answer>,
    imports : ImportTable<Import>,
}

fn client_hooks_of_payload(payload : Payload::Reader,
                           rpc_chan : &std::comm::Sender<RpcEvent>) -> Vec<Option<~ClientHook:Send>> {
    let mut result = Vec::new();
    let cap_table = payload.get_cap_table();
    for ii in range(0, cap_table.size()) {
        match cap_table[ii].which() {
            Some(CapDescriptor::None(())) => {
                result.push(None)
            }
            Some(CapDescriptor::SenderHosted(id)) => {
                result.push(Some(
                        (box ImportClient {
                                channel : rpc_chan.clone(),
                                import_id : id})
                            as ~ClientHook:Send));
            }
            Some(CapDescriptor::SenderPromise(_id)) => {
                println!("warning: SenderPromise is unimplemented");
                result.push(None);
            }
            Some(CapDescriptor::ReceiverHosted(_id)) => {
                fail!()
            }
            Some(CapDescriptor::ReceiverAnswer(promised_answer)) => {
                result.push(Some(
                        (box PromisedAnswerClient {
                                rpc_chan : rpc_chan.clone(),
                                ops : get_pipeline_ops(promised_answer),
                                answer_id : promised_answer.get_question_id(),} as ~ClientHook:Send)));
            }
            Some(CapDescriptor::ThirdPartyHosted(_)) => {
                fail!()
            }
            None => { fail!("unknown cap descriptor")}
        }
    }
    result
}

fn populate_cap_table(message : &mut OwnedSpaceMessageReader,
                      rpc_chan : &std::comm::Sender<RpcEvent>) {
    let mut the_cap_table : Vec<Option<~ClientHook:Send>> = Vec::new();
    {
        let root = message.get_root::<Message::Reader>();

        match root.which() {
            Some(Message::Return(ret)) => {
                match ret.which() {
                    Some(Return::Results(payload)) => {
                        the_cap_table = client_hooks_of_payload(payload, rpc_chan);
                    }
                    Some(Return::Exception(_e)) => {
                    }
                    _ => {}
                }

            }
            Some(Message::Call(call)) => {
               the_cap_table = client_hooks_of_payload(call.get_params(), rpc_chan);
            }
            Some(Message::Unimplemented(_)) => {
            }
            Some(Message::Abort(_exc)) => {
            }
            None => {
            }
            _ => {
            }
        }
    }
    message.init_cap_table(the_cap_table);
}

fn get_pipeline_ops(promised_answer : PromisedAnswer::Reader) -> Vec<PipelineOp::Type> {
    let mut result = Vec::new();
    let transform = promised_answer.get_transform();
    for ii in range(0, transform.size()) {
        match transform[ii].which() {
            Some(PromisedAnswer::Op::Noop(())) => result.push(PipelineOp::Noop),
            Some(PromisedAnswer::Op::GetPointerField(idx)) => result.push(PipelineOp::GetPointerField(idx)),
            None => {}
        }
    }
    return result;
}

impl RpcConnectionState {
    pub fn new() -> RpcConnectionState {
        RpcConnectionState {
            exports : ExportTable::new(),
            questions : ExportTable::new(),
            answers : ImportTable::new(),
            imports : ImportTable::new(),
        }
    }

    pub fn run<T : std::io::Reader + Send, U : std::io::Writer + Send, V : SturdyRefRestorer + Send>(
        self, inpipe: T, outpipe: U, restorer : V)
         -> std::comm::Sender<RpcEvent> {

        let (result_rpc_chan, port) = std::comm::channel::<RpcEvent>();

        let listener_chan = result_rpc_chan.clone();

        spawn(proc() {
                let mut r = inpipe;
                loop {
                    match serialize::new_reader(
                        &mut r,
                        *ReaderOptions::new().fail_fast(false)) {
                        Err(_e) => { assert!(listener_chan.send_opt(ShutdownEvent).is_ok()); break; }
                        Ok(message) => {
                            listener_chan.send(IncomingMessage(box message));
                        }
                    }
                }
            });


        let (writer_chan, writer_port) = std::comm::channel::<~MallocMessageBuilder>();

        let writer_rpc_chan = result_rpc_chan.clone();

        spawn(proc() {
                let mut w = outpipe;
                loop {
                    let message = match writer_port.recv_opt() {
                        Err(_) => break,
                        Ok(m) => m,
                    };
                    serialize::write_message(&mut w, message).unwrap();
                    writer_rpc_chan.send(AnswerSent(message));
                }
            });

        let rpc_chan = result_rpc_chan.clone();

        spawn(proc() {
                let RpcConnectionState {mut questions, mut exports, mut answers, imports : _imports} = self;
                loop {
                    match port.recv() {
                        AnswerSent(mut message) => {
                            let root = message.get_root::<Message::Builder>();
                            let answer_id_opt = match root.which() {
                                Some(Message::Return(ret)) => {
                                    Some(ret.get_answer_id())
                                }
                                _ => {None}
                            };
                            match answer_id_opt {
                                Some(answer_id) => {
                                    answers.slots.get_mut(&answer_id).sent(message)
                                }
                                _ => {}
                            }

                        }
                        IncomingMessage(mut message) => {
                            enum MessageReceiver {
                                Nobody,
                                QuestionReceiver(QuestionId),
                                ExportReceiver(ExportId),
                                PromisedAnswerReceiver(AnswerId, Vec<PipelineOp::Type>),
                            }

                            populate_cap_table(message, &rpc_chan);
                            let root = message.get_root::<Message::Reader>();
                            let receiver = match root.which() {
                                Some(Message::Unimplemented(_)) => {
                                    println!("unimplemented");
                                    Nobody
                                }
                                Some(Message::Abort(exc)) => {
                                    println!("abort: {}", exc.get_reason());
                                    Nobody
                                }
                                Some(Message::Call(call)) => {
                                    match call.get_target().which() {
                                        Some(MessageTarget::ImportedCap(import_id)) => {
                                            ExportReceiver(import_id)
                                        }
                                        Some(MessageTarget::PromisedAnswer(promised_answer)) => {
                                            PromisedAnswerReceiver(
                                                promised_answer.get_question_id(),
                                                get_pipeline_ops(promised_answer))
                                        }
                                        None => {
                                            fail!("call targets something else");
                                        }
                                    }
                                }

                                Some(Message::Return(ret)) => {
                                    QuestionReceiver(ret.get_answer_id())
                                }
                                Some(Message::Finish(_finish)) => {
                                    println!("finish");
                                    Nobody
                                }
                                Some(Message::Resolve(_resolve)) => {
                                    println!("resolve");
                                    Nobody
                                }
                                Some(Message::Release(rel)) => {
                                    assert!(rel.get_reference_count() == 1);
                                    exports.erase(rel.get_id());
                                    Nobody
                                }
                                Some(Message::Disembargo(_dis)) => {
                                    println!("disembargo");
                                    Nobody
                                }
                                Some(Message::Save(_save)) => {
                                    Nobody
                                }
                                Some(Message::Restore(restore)) => {
                                    let clienthook = restorer.restore(restore.get_object_id()).unwrap();
                                    let idx = exports.push(Export { hook : clienthook.copy() });

                                    let answer_id = restore.get_question_id();
                                    let mut message = ~MallocMessageBuilder::new_default();
                                    {
                                        let root : Message::Builder = message.init_root();
                                        let ret = root.init_return();
                                        ret.set_answer_id(answer_id);
                                        let payload = ret.init_results();
                                        payload.init_cap_table(1);
                                        payload.get_cap_table()[0].set_sender_hosted(idx as u32);
                                        payload.get_content().set_as_capability(clienthook);

                                    }
                                    answers.slots.insert(answer_id, Answer::new());

                                    writer_chan.send(message);
                                    Nobody
                                }
                                Some(Message::Delete(_delete)) => {
                                    Nobody
                                }
                                Some(Message::Provide(_provide)) => {
                                    Nobody
                                }
                                Some(Message::Accept(_accept)) => {
                                    Nobody
                                }
                                Some(Message::Join(_join)) => {
                                    Nobody
                                }
                                None => {
                                    println!("unknown message");
                                    Nobody
                                }
                            };

                            fn get_call_ids(message : &OwnedSpaceMessageReader)
                                                        -> (QuestionId, u64, u16) {
                                let root : Message::Reader = message.get_root();
                                match root.which() {
                                    Some(Message::Call(call)) =>
                                        (call.get_question_id(), call.get_interface_id(), call.get_method_id()),
                                    _ => fail!(),
                                }
                            }

                            match receiver {
                                Nobody => {}
                                QuestionReceiver(id) => {
                                    match questions.slots.get(id as uint) {
                                        &Some(ref q) => {
                                            q.chan.send_opt(
                                                ~RpcResponse::new(message) as ~ResponseHook:Send).is_ok();
                                        }
                                        &None => {
                                            // XXX Todo
                                            fail!()
                                        }
                                    }
                                }
                                ExportReceiver(id) => {
                                    let (answer_id, interface_id, method_id) = get_call_ids(message);
                                    let context =
                                        ~RpcCallContext::new(message, rpc_chan.clone()) as ~CallContextHook:Send;

                                    answers.slots.insert(answer_id, Answer::new());
                                    match exports.slots.get(id as uint) {
                                        &Some(ref ex) => {
                                            ex.hook.call(interface_id, method_id, context);
                                        }
                                        &None => {
                                            // XXX todo
                                            fail!()
                                        }

                                    }
                                }
                                PromisedAnswerReceiver(id, ops) => {
                                    let (answer_id, interface_id, method_id) = get_call_ids(message);
                                    let context =
                                        ~RpcCallContext::new(message, rpc_chan.clone()) as ~CallContextHook:Send;

                                    answers.slots.insert(answer_id, Answer::new());
                                    answers.slots.get_mut(&id).receive(interface_id, method_id, ops, context);

                                }
                            }


                        }
                        Outgoing(OutgoingMessage { message : mut m,
                                                   answer_chan,
                                                   question_chan} ) => {
                            let root = m.get_root::<Message::Builder>();
                            // add a question to the question table
                            match root.which() {
                                Some(Message::Return(_)) => {}
                                Some(Message::Call(call)) => {
                                    let id = questions.push(Question {is_awaiting_return : true,
                                                                      chan : answer_chan});
                                    call.set_question_id(id);
                                    question_chan.send_opt(id).is_ok();
                                }
                                Some(Message::Restore(res)) => {
                                    let id = questions.push(Question {is_awaiting_return : true,
                                                                      chan : answer_chan});
                                    res.set_question_id(id);
                                    question_chan.send_opt(id).is_ok();
                                }
                                _ => {
                                    fail!("NONE OF THOSE");
                                }
                            }

                            // send
                            writer_chan.send(m);
                        }
                        OutgoingDeferred(OutgoingMessage { message : mut m,
                                                           answer_chan,
                                                           question_chan: _}, answer_id, ops ) => {

                            let root = m.get_root::<Message::Builder>();
                            let (interface_id, method_id) = match root.which() {
                                Some(Message::Call(call)) => {
                                    (call.get_interface_id(), call.get_method_id())
                                }
                                _ => {
                                    fail!("NONE OF THOSE");
                                }
                            };

                            let context =
                                (~PromisedAnswerRpcCallContext::new(m, rpc_chan.clone(), answer_chan))
                                as ~CallContextHook:Send;


                            answers.slots.get_mut(&answer_id).receive(interface_id, method_id, ops, context);
                        }

                        NewLocalServer(clienthook, export_chan) => {
                            let export_id = exports.push(Export { hook : clienthook });
                            export_chan.send(export_id);
                        }
                        ReturnEvent(message) => {
                            writer_chan.send(message);
                        }
                        ShutdownEvent => {
                            break;
                        }
                    }
                }});
        return result_rpc_chan;
    }
}

// HACK
pub enum OwnedCapDescriptor {
    NoDescriptor,
    SenderHosted(ExportId),
    SenderPromise(ExportId),
    ReceiverHosted(ImportId),
    ReceiverAnswer(QuestionId, Vec<PipelineOp::Type>),
}

pub struct ImportClient {
    channel : std::comm::Sender<RpcEvent>,
    pub import_id : ImportId,
}

impl ClientHook for ImportClient {
    fn copy(&self) -> ~ClientHook:Send {
        (box ImportClient {channel : self.channel.clone(),
                           import_id : self.import_id}) as ~ClientHook:Send
    }

    fn new_call(&self, interface_id : u64, method_id : u16,
                _size_hint : Option<common::MessageSize>)
                -> capability::Request<AnyPointer::Builder, AnyPointer::Reader, AnyPointer::Pipeline> {
        let mut message = box MallocMessageBuilder::new(*BuilderOptions::new().fail_fast(false));
        {
            let root : Message::Builder = message.get_root();
            let call = root.init_call();
            call.set_interface_id(interface_id);
            call.set_method_id(method_id);
            let target = call.init_target();
            target.set_imported_cap(self.import_id);
        }
        let hook = box RpcRequest { channel : self.channel.clone(),
                                    message : message };
        Request::new(hook as ~RequestHook)
    }

    fn call(&self, _interface_id : u64, _method_id : u16, _context : ~CallContextHook) {
        fail!()
    }

    fn get_descriptor(&self) -> ~std::any::Any {
        (box ReceiverHosted(self.import_id)) as ~std::any::Any
    }
}

pub struct PipelineClient {
    channel : std::comm::Sender<RpcEvent>,
    pub ops : Vec<PipelineOp::Type>,
    pub question_id : QuestionId,
}

impl ClientHook for PipelineClient {
    fn copy(&self) -> ~ClientHook:Send {
        (~PipelineClient { channel : self.channel.clone(),
                           ops : self.ops.clone(),
                           question_id : self.question_id,
            }) as ~ClientHook:Send
    }

    fn new_call(&self, interface_id : u64, method_id : u16,
                _size_hint : Option<common::MessageSize>)
                -> capability::Request<AnyPointer::Builder, AnyPointer::Reader, AnyPointer::Pipeline> {
        let mut message = box MallocMessageBuilder::new(*BuilderOptions::new().fail_fast(false));
        {
            let root : Message::Builder = message.get_root();
            let call = root.init_call();
            call.set_interface_id(interface_id);
            call.set_method_id(method_id);
            let target = call.init_target();
            let promised_answer = target.init_promised_answer();
            promised_answer.set_question_id(self.question_id);
            let transform = promised_answer.init_transform(self.ops.len());
            for ii in range(0, self.ops.len()) {
                match self.ops.as_slice()[ii] {
                    PipelineOp::Noop => transform[ii].set_noop(()),
                    PipelineOp::GetPointerField(idx) => transform[ii].set_get_pointer_field(idx),
                }
            }
        }
        let hook = box RpcRequest { channel : self.channel.clone(),
                                    message : message };
        Request::new(hook as ~RequestHook)
    }

    fn call(&self, _interface_id : u64, _method_id : u16, _context : ~CallContextHook) {
        fail!()
    }

    fn get_descriptor(&self) -> ~std::any::Any {
        (box ReceiverAnswer(self.question_id, self.ops.clone())) as ~std::any::Any
    }
}

pub struct PromisedAnswerClient {
    rpc_chan : std::comm::Sender<RpcEvent>,
    ops : Vec<PipelineOp::Type>,
    answer_id : AnswerId,
}

impl ClientHook for PromisedAnswerClient {
    fn copy(&self) -> ~ClientHook:Send {
        (~PromisedAnswerClient { rpc_chan : self.rpc_chan.clone(),
                                 ops : self.ops.clone(),
                                 answer_id : self.answer_id,
            }) as ~ClientHook:Send
    }

    fn new_call(&self, interface_id : u64, method_id : u16,
                _size_hint : Option<common::MessageSize>)
                -> capability::Request<AnyPointer::Builder, AnyPointer::Reader, AnyPointer::Pipeline> {
        let mut message = box MallocMessageBuilder::new(*BuilderOptions::new().fail_fast(false));
        {
            let root : Message::Builder = message.get_root();
            let call = root.init_call();
            call.set_interface_id(interface_id);
            call.set_method_id(method_id);
        }

        let hook = box PromisedAnswerRpcRequest { rpc_chan : self.rpc_chan.clone(),
                                                  message : message,
                                                  answer_id : self.answer_id,
                                                  ops : self.ops.clone() };
        Request::new(hook as ~RequestHook)
    }

    fn call(&self, _interface_id : u64, _method_id : u16, _context : ~CallContextHook) {
        fail!()
    }

    fn get_descriptor(&self) -> ~std::any::Any {
        fail!()
    }
}


fn write_outgoing_cap_table(rpc_chan : &std::comm::Sender<RpcEvent>, message : &mut MallocMessageBuilder) {
    fn write_payload(rpc_chan : &std::comm::Sender<RpcEvent>, cap_table : & [~std::any::Any],
                     payload : Payload::Builder) {
        let new_cap_table = payload.init_cap_table(cap_table.len());
        for ii in range(0, cap_table.len()) {
            match cap_table[ii].as_ref::<OwnedCapDescriptor>() {
                Some(&NoDescriptor) => {}
                Some(&ReceiverHosted(import_id)) => {
                    new_cap_table[ii].set_receiver_hosted(import_id);
                }
                Some(&ReceiverAnswer(question_id,ref ops)) => {
                    let promised_answer = new_cap_table[ii].init_receiver_answer();
                    promised_answer.set_question_id(question_id);
                    let transform = promised_answer.init_transform(ops.len());
                    for ii in range(0, ops.len()) {
                        match ops.as_slice()[ii] {
                            PipelineOp::Noop => transform[ii].set_noop(()),
                            PipelineOp::GetPointerField(idx) => transform[ii].set_get_pointer_field(idx),
                        }
                    }
                }
                Some(&SenderHosted(export_id)) => {
                    new_cap_table[ii].set_sender_hosted(export_id);
                }
                None => {
                    match cap_table[ii].as_ref::<~ClientHook:Send>() {
                        Some(clienthook) => {
                            let (chan, port) = std::comm::channel::<ExportId>();
                            rpc_chan.send(NewLocalServer(clienthook.copy(), chan));
                            let idx = port.recv();
                            new_cap_table[ii].set_sender_hosted(idx);
                        }
                        None => fail!("noncompliant client hook"),
                    }
                }
                _ => {}
            }
        }
    }

    let cap_table = {
        let mut caps = Vec::new();
        for cap in message.get_cap_table().iter() {
            match cap {
                &Some(ref client_hook) => {
                    caps.push(client_hook.get_descriptor())
                }
                &None => {}
            }
        }
        caps
    };
    let root : Message::Builder = message.get_root();
    match root.which() {
        Some(Message::Call(call)) => {
            write_payload(rpc_chan, cap_table.as_slice(), call.get_params())
        }
        Some(Message::Return(ret)) => {
            match ret.which() {
                Some(Return::Results(payload)) => {
                    write_payload(rpc_chan, cap_table.as_slice(), payload);
                }
                _ => {}
            }
        }
        _ => {}
    }
}

pub struct RpcResponse {
    message : ~OwnedSpaceMessageReader,
}

impl RpcResponse {
    pub fn new(message : ~OwnedSpaceMessageReader) -> RpcResponse {
        RpcResponse { message : message }
    }
}

impl ResponseHook for RpcResponse {
    fn get<'a>(&'a mut self) -> AnyPointer::Reader<'a> {
        self.message.get_root_internal()
    }
}

pub struct RpcRequest {
    channel : std::comm::Sender<RpcEvent>,
    message : ~MallocMessageBuilder,
}

impl RequestHook for RpcRequest {
    fn message<'a>(&'a mut self) -> &'a mut MallocMessageBuilder {
        &mut *self.message
    }
    fn send(~self) -> ResultFuture<AnyPointer::Reader, AnyPointer::Pipeline> {
        let ~RpcRequest { channel, mut message } = self;
        write_outgoing_cap_table(&channel, message);

        let (outgoing, answer_port, question_port) = RpcEvent::new_outgoing(message);
        channel.send(Outgoing(outgoing));

        let question_id = question_port.recv();

        let pipeline = ~RpcPipeline {channel : channel, question_id : question_id};
        let typeless = AnyPointer::Pipeline::new(pipeline as ~PipelineHook);

        ResultFuture {answer_port : answer_port, answer_result : Err(()) /* XXX */,
                       pipeline : typeless  }
    }
}

pub struct PromisedAnswerRpcRequest {
    rpc_chan : std::comm::Sender<RpcEvent>,
    message : ~MallocMessageBuilder,
    answer_id : AnswerId,
    ops : Vec<PipelineOp::Type>,
}

impl RequestHook for PromisedAnswerRpcRequest {
    fn message<'a>(&'a mut self) -> &'a mut MallocMessageBuilder {
        &mut *self.message
    }
    fn send(~self) -> ResultFuture<AnyPointer::Reader, AnyPointer::Pipeline> {
        let ~PromisedAnswerRpcRequest { rpc_chan, message, answer_id, ops } = self;
        let (outgoing, answer_port, _question_port) = RpcEvent::new_outgoing(message);
        rpc_chan.send(OutgoingDeferred(outgoing, answer_id, ops));

        let pipeline = ~PromisedAnswerRpcPipeline;
        let typeless = AnyPointer::Pipeline::new(pipeline as ~PipelineHook);

        ResultFuture {answer_port : answer_port, answer_result : Err(()) /* XXX */,
                       pipeline : typeless  }
    }
}


pub struct RpcPipeline {
    channel : std::comm::Sender<RpcEvent>,
    question_id : ExportId,
}

impl PipelineHook for RpcPipeline {
    fn copy(&self) -> ~PipelineHook {
        (~RpcPipeline { channel : self.channel.clone(),
                        question_id : self.question_id }) as ~PipelineHook
    }
    fn get_pipelined_cap(&self, ops : Vec<PipelineOp::Type>) -> ~ClientHook:Send {
        (~PipelineClient { channel : self.channel.clone(),
                           ops : ops,
                           question_id : self.question_id,
        }) as ~ClientHook:Send
    }
}

pub struct PromisedAnswerRpcPipeline;

impl PipelineHook for PromisedAnswerRpcPipeline {
    fn copy(&self) -> ~PipelineHook {
        (~PromisedAnswerRpcPipeline) as ~PipelineHook
    }
    fn get_pipelined_cap(&self, _ops : Vec<PipelineOp::Type>) -> ~ClientHook:Send {
        fail!()
    }
}

pub struct Aborter {
    succeeded : bool,
    answer_id : AnswerId,
    rpc_chan : std::comm::Sender<RpcEvent>,
}

impl Drop for Aborter {
    fn drop(&mut self) {
        if !self.succeeded {
            let mut results_message = ~MallocMessageBuilder::new_default();
            {
                let root : Message::Builder = results_message.init_root();
                let ret = root.init_return();
                ret.set_answer_id(self.answer_id);
                let exc = ret.init_exception();
                exc.set_reason("aborted");
            }
            self.rpc_chan.send(ReturnEvent(results_message));
        }
    }
}

pub struct RpcCallContext {
    params_message : ~OwnedSpaceMessageReader,
    results_message : ~MallocMessageBuilder,
    rpc_chan : std::comm::Sender<RpcEvent>,
    aborter : Aborter,
}

impl RpcCallContext {
    pub fn new(params_message : ~OwnedSpaceMessageReader,
               rpc_chan : std::comm::Sender<RpcEvent>) -> RpcCallContext {
        let answer_id = {
            let root : Message::Reader = params_message.get_root();
            match root.which() {
                Some(Message::Call(call)) => {
                    call.get_question_id()
                }
                _ => fail!(),
            }
        };
        let mut results_message = ~MallocMessageBuilder::new(*BuilderOptions::new().fail_fast(false));
        {
            let root : Message::Builder = results_message.init_root();
            let ret = root.init_return();
            ret.set_answer_id(answer_id);
            ret.init_results();
        }
        RpcCallContext {
            params_message : params_message,
            results_message : results_message,
            rpc_chan : rpc_chan.clone(),
            aborter : Aborter { succeeded : false, answer_id : answer_id, rpc_chan : rpc_chan},
        }
    }
}

impl CallContextHook for RpcCallContext {
    fn get<'a>(&'a mut self) -> (AnyPointer::Reader<'a>, AnyPointer::Builder<'a>) {

        let params = {
            let root : Message::Reader = self.params_message.get_root();
            match root.which() {
                Some(Message::Call(call)) => {
                    call.get_params().get_content()
                }
                _ => fail!(),
            }
        };

        let results = {
            let root : Message::Builder = self.results_message.get_root();
            match root.which() {
                Some(Message::Return(ret)) => {
                    match ret.which() {
                        Some(Return::Results(results)) => {
                            results.get_content()
                        }
                        _ => fail!(),
                    }
                }
                _ => fail!(),
            }
        };

        (params, results)
    }
    fn fail(mut ~self) {
        self.aborter.succeeded = false;
    }

    fn done(~self) {
        let ~RpcCallContext { params_message : _, mut results_message, rpc_chan, mut aborter} = self;
        aborter.succeeded = true;
        write_outgoing_cap_table(&rpc_chan, results_message);

        rpc_chan.send(ReturnEvent(results_message));
    }
}

pub struct LocalResponse {
    message : ~MallocMessageBuilder,
}

impl LocalResponse {
    pub fn new(message : ~MallocMessageBuilder) -> LocalResponse {
        LocalResponse { message : message }
    }
}

impl ResponseHook for LocalResponse {
    fn get<'a>(&'a mut self) -> AnyPointer::Reader<'a> {
        self.message.get_root_internal().as_reader()
    }
}


pub struct PromisedAnswerRpcCallContext {
    params_message : ~MallocMessageBuilder,
    results_message : ~MallocMessageBuilder,
    rpc_chan : std::comm::Sender<RpcEvent>,
    answer_chan : std::comm::Sender<~ResponseHook:Send>,
}

impl PromisedAnswerRpcCallContext {
    pub fn new(params_message : ~MallocMessageBuilder,
               rpc_chan : std::comm::Sender<RpcEvent>,
               answer_chan : std::comm::Sender<~ResponseHook:Send>)
               -> PromisedAnswerRpcCallContext {


        let mut results_message = ~MallocMessageBuilder::new(*BuilderOptions::new().fail_fast(false));
        {
            let root : Message::Builder = results_message.init_root();
            let ret = root.init_return();
            ret.init_results();
        }
        PromisedAnswerRpcCallContext {
            params_message : params_message,
            results_message : results_message,
            rpc_chan : rpc_chan,
            answer_chan : answer_chan,
        }
    }
}

impl CallContextHook for PromisedAnswerRpcCallContext {
    fn get<'a>(&'a mut self) -> (AnyPointer::Reader<'a>, AnyPointer::Builder<'a>) {

        let params = {
            let root : Message::Builder = self.params_message.get_root();
            match root.which() {
                Some(Message::Call(call)) => {
                    call.get_params().get_content().as_reader()
                }
                _ => fail!(),
            }
        };

        let results = {
            let root : Message::Builder = self.results_message.get_root();
            match root.which() {
                Some(Message::Return(ret)) => {
                    match ret.which() {
                        Some(Return::Results(results)) => {
                            results.get_content()
                        }
                        _ => fail!(),
                    }
                }
                _ => fail!(),
            }
        };

        (params, results)
    }
    fn fail(~self) {
        let ~PromisedAnswerRpcCallContext {
            params_message : _, mut results_message, rpc_chan : _, answer_chan} = self;

        let message : Message::Builder = results_message.get_root();
        match message.which() {
            Some(Message::Return(ret)) => {
                let exc = ret.init_exception();
                exc.set_reason("aborted");
            }
            _ => fail!(),
        }

        answer_chan.send(~LocalResponse::new(results_message) as ~ResponseHook:Send);

    }

    fn done(~self) {
        let ~PromisedAnswerRpcCallContext {
            params_message : _, results_message, rpc_chan : _, answer_chan} = self;

        answer_chan.send(~LocalResponse::new(results_message) as ~ResponseHook:Send);
    }
}


pub struct OutgoingMessage {
    message : ~MallocMessageBuilder,
    answer_chan : std::comm::Sender<~ResponseHook:Send>,
    question_chan : std::comm::Sender<QuestionId>,
}


pub enum RpcEvent {
    AnswerSent(~MallocMessageBuilder),
    IncomingMessage(~serialize::OwnedSpaceMessageReader),
    Outgoing(OutgoingMessage),
    OutgoingDeferred(OutgoingMessage, AnswerId, Vec<PipelineOp::Type>),
    NewLocalServer(~ClientHook:Send, std::comm::Sender<ExportId>),
    ReturnEvent(~MallocMessageBuilder),
    ShutdownEvent,
}


impl RpcEvent {
    pub fn new_outgoing(message : ~MallocMessageBuilder)
                        -> (OutgoingMessage, std::comm::Receiver<~ResponseHook:Send>,
                            std::comm::Receiver<ExportId>) {
        let (answer_chan, answer_port) = std::comm::channel::<~ResponseHook:Send>();

        let (question_chan, question_port) = std::comm::channel::<ExportId>();

        (OutgoingMessage{ message : message,
                          answer_chan : answer_chan,
                          question_chan : question_chan },
         answer_port,
         question_port)
    }
}

