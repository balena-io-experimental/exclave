// The UnitManager contains all units that are Selected.  This includes
// units that are Active.
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use config::Config;
use unit::{UnitName, UnitKind, UnitActivateError, UnitDeactivateError, UnitSelectError, UnitDeselectError};
use unitbroadcaster::{UnitBroadcaster, UnitEvent, UnitStatusEvent, UnitStatus, LogEntry};
use units::interface::{Interface, InterfaceDescription};
use units::jig::{Jig, JigDescription};
use units::scenario::{Scenario, ScenarioDescription};
use units::test::{Test, TestDescription};

macro_rules! load {
    ($slf:ident, $dest:ident, $desc:ident) => {
        {
            // If the item exists in the array already, then it is active and will be deactivated first.
            if $slf.$dest.borrow_mut().contains_key($desc.id()) {
                // Deactivate it before unloading
                $slf.deactivate($desc.id(), "reloading");
                $slf.deselect($desc.id(), "reloading");
            };
            // "Select" the Interface, which means we can activate it later on.
            match $desc.select($slf, &*$slf.cfg.lock().unwrap()) {
                Ok(o) => {
                    let new_item = Rc::new(RefCell::new(o));
                    // Announce the fact that the interface was loaded successfully.
                    $slf.bc
                        .broadcast(&UnitEvent::Status(UnitStatusEvent::new_loaded($desc.id())));

                    $slf.$dest.borrow_mut().insert($desc.id().clone(), new_item.clone());
                    Ok($desc.id().clone())
                }
                Err(e) => {
                    $slf.bc.broadcast(
                        &UnitEvent::Status(UnitStatusEvent::new_unit_incompatible(
                            $desc.id(),
                            format!("{}", e),
                        )),
                    );
                    Err(())
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum FieldType {
    Name,
    Description,
}

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &FieldType::Name => write!(f, "name"),
            &FieldType::Description => write!(f, "description"),
        }
    }
}

/// Messages for Library -> Unit communication
#[derive(Debug)]
pub enum ManagerStatusMessage {
    /// Return the first name of the jig we're running on.
    Jig(UnitName /* Name of the jig */),

    /// Return a list of known scenarios.
    Scenarios(Vec<UnitName>),

    /// Return the currently-selected scenario, if any
    Scenario(Option<UnitName>),

    /// Return a list of tests in a scenario.
    Tests(UnitName /* Scenario name */, Vec<UnitName> /* List of tests */),

    /// Greeting identifying the server.
    Hello(String /* Server identification name */),

    /// Describes a Type of a particular Field on a given Unit
    Describe(UnitKind, FieldType, String /* UnitId */, String /* Value */),

    /// A log message from one of the units, or the system itself.
    Log(LogEntry),
}

/// Messages for Unit -> Library communication
#[derive(PartialEq, Eq, Hash, Debug, Clone)]
pub enum ManagerControlMessageContents {
    /// Get the current Jig
    Jig,

    /// Get a list of compatible, Selected scenarios.
    Scenarios,

    /// Select a specific scenario.
    Scenario(UnitName /* Scenario name */),

    /// Get a list of tests, either from the current scenario (None) or a specific scenario (Some)
    Tests(Option<UnitName>),

    /// An error message from a particular interface.
    Error(String /* Error message contents */),

    /// Sent to a unit when it is first loaded, including "HELLO" messages.
    InitialGreeting,

    /// Indicates the child object terminated unexpectedly.
    ChildExited,

    /// Client sent an unimplemented message.
    Unimplemented(String /* verb */, String /* rest of line */),

    /// Send an INFO message to the logging system
    Log(String /* log message */),

    /// Send an ERROR message to the logging system
    LogError(String /* log message */),

    /// Start running a scenario, or the default scenario if None
    Start(Option<UnitName>),
}

#[derive(PartialEq, Eq, Hash, Debug, Clone)]
pub struct ManagerControlMessage {
    sender: UnitName,
    contents: ManagerControlMessageContents,
}

impl ManagerControlMessage {
    pub fn new(id: &UnitName, contents: ManagerControlMessageContents) -> Self {
        ManagerControlMessage {
            sender: id.clone(),
            contents: contents,
        }
    }
}

pub struct UnitManager {
    cfg: Arc<Mutex<Config>>,
    bc: UnitBroadcaster,

    /// Selected Interfaces, available for activation.
    interfaces: RefCell<HashMap<UnitName, Rc<RefCell<Interface>>>>,

    /// Selected Jigs, available for activation.
    jigs: RefCell<HashMap<UnitName, Rc<RefCell<Jig>>>>,

    /// Selected Scenarios, available for activation.
    scenarios: Rc<RefCell<HashMap<UnitName, Rc<RefCell<Scenario>>>>>,

    /// Selected Tests, available for activation.
    tests: Rc<RefCell<HashMap<UnitName, Rc<RefCell<Test>>>>>,

    /// Prototypical message sender that will be cloned and passed to each new unit.
    control_sender: Sender<ManagerControlMessage>,

    /// The currently-selected Scenario, if any
    current_scenario: Rc<RefCell<Option<Rc<RefCell<Scenario>>>>>,

    /// The currently-selected Jig, if any
    current_jig: Rc<RefCell<Option<Rc<RefCell<Jig>>>>>,
}

impl UnitManager {
    pub fn new(broadcaster: &UnitBroadcaster, config: &Arc<Mutex<Config>>) -> Self {
        let (sender, receiver) = channel();

        let monitor_broadcaster = broadcaster.clone();
        thread::spawn(move || Self::control_message_monitor(receiver, monitor_broadcaster));

        UnitManager {
            cfg: config.clone(),
            bc: broadcaster.clone(),

            interfaces: RefCell::new(HashMap::new()),
            jigs: RefCell::new(HashMap::new()),
            scenarios: Rc::new(RefCell::new(HashMap::new())),
            tests: Rc::new(RefCell::new(HashMap::new())),

            current_scenario: Rc::new(RefCell::new(None)),
            current_jig: Rc::new(RefCell::new(None)),

            control_sender: sender,
        }
    }

    /// Runs in a separate thread and consolidates control messages
    fn control_message_monitor(receiver: Receiver<ManagerControlMessage>, broadcaster: UnitBroadcaster) {
        while let Ok(msg) = receiver.recv() {
            broadcaster.broadcast(&UnitEvent::ManagerRequest(msg));
        }
    }

    pub fn get_control_channel(&self) -> Sender<ManagerControlMessage> {
        self.control_sender.clone()
    }

    pub fn load_interface(&self, description: &InterfaceDescription) -> Result<UnitName, ()> {
        load!(self, interfaces, description)
    }

    pub fn load_test(&self, desceription: &TestDescription) -> Result<UnitName, ()> {
        load!(self, tests, desceription)
    }

    pub fn load_jig(&self, desceription: &JigDescription) -> Result<UnitName, ()> {
        load!(self, jigs, desceription)
    }

    pub fn load_scenario(&self, desceription: &ScenarioDescription) -> Result<UnitName, ()> {
        load!(self, scenarios, desceription)
    }

    pub fn select(&self, id: &UnitName) {
        let result = match *id.kind() {
            UnitKind::Interface => self.select_interface(id),
            UnitKind::Jig => self.select_jig(id),
            UnitKind::Scenario => self.select_scenario(id),
            UnitKind::Test => self.select_test(id),
            UnitKind::Internal => Ok(()),
        };

        // Announce that the interface was successfully started.
        match result {
            Ok(_) => self.bc.broadcast(&UnitEvent::Status(UnitStatusEvent::new_active(id))),
            Err(e) =>
               self.bc.broadcast(
                    &UnitEvent::Status(UnitStatusEvent::new_active_failed(id, format!("unable to deactivate: {}", e)))),
        }
    }

    pub fn select_scenario(&self, id: &UnitName) -> Result<(), UnitSelectError> {
        let new_scenario = match self.scenarios.borrow().get(id) {
            Some(s) => s.clone(),
            None => return Err(UnitSelectError::UnitNotFound),
        };

        // If there is an existing current scenario, check to see if the ID matches.
        // If so, there is nothing to do.
        // If not, deselect it.
        // There Can Only Be One.
        let should_deselect = if let Some(ref old_scenario) = *self.current_scenario.borrow() {
            if old_scenario.borrow().id() == id {
                // Units match, so do nothing.
                return Ok(());
            }
            true
        } else {
            false
        };

        if should_deselect {
            self.deselect(id, "switching to a new scenario");
        }
        
        // Select this scenario.
        new_scenario.borrow_mut().select()?;
        *self.current_scenario.borrow_mut() = Some(new_scenario.clone());
        self.bc
            .broadcast(&UnitEvent::Status(UnitStatusEvent::new_active(id)));
        Ok(())
    }

    fn select_jig(&self, id: &UnitName) -> Result<(), UnitSelectError> {
        unimplemented!();
    }

    fn select_test(&self, id: &UnitName) -> Result<(), UnitSelectError> { 
        unimplemented!();
    }

    fn select_interface(&self, id: &UnitName) -> Result<(), UnitSelectError> {
        unimplemented!();
    }

    pub fn deselect(&self, id: &UnitName, reason: &str) {
        // Remove the item from its associated Rc array.
        // Note that because these are Rcs, they may live on for a little while
        // longer as references in other objects.
        let result = match id.kind() {
            &UnitKind::Interface => self.deselect_interface(id),
            &UnitKind::Test => self.deselect_test(id),
            &UnitKind::Scenario => self.deselect_scenario(id),
            &UnitKind::Jig => self.deselect_jig(id),
            &UnitKind::Internal => Ok(()),
        };

        // A not-okay result is fine, it just means we couldn't find the unit.
        if result.is_ok() {
            self.bc.broadcast(&UnitEvent::Status(UnitStatusEvent::new_deselected(id, reason.to_owned())));
        }
    }

    fn deselect_test(&self, _id: &UnitName) -> Result<(), UnitDeselectError> {
        unimplemented!();
    }

    fn deselect_interface(&self, _id: &UnitName) -> Result<(), UnitDeselectError> {
        unimplemented!();
    }

    fn deselect_jig(&self, id: &UnitName) -> Result<(), UnitDeselectError> {
        // If the specified jig isn't the current jig, then there's nothing to do.
        let mut current_jig_opt = self.current_jig.borrow_mut();

        let current_jig = match *current_jig_opt {
            None => return Ok(()),
            Some(ref s) => {
                let current_jig = s.borrow();
                if current_jig.id() != id {
                    return Ok(());
                }
                s.clone()
            }
        };

        // If there is a default scenario, make sure it's deselected.
        if let Some(new_scenario_id) = current_jig.borrow().default_scenario().clone() {
            self.deselect(&new_scenario_id, "jig is deselecting");
        }

        current_jig.borrow_mut().deselect()?;
        *current_jig_opt = None;
        Ok(())
    }

    fn deselect_scenario(&self, id: &UnitName) -> Result<(), UnitDeselectError> {
        // If the specified scenario isn't the current scenario, then there's nothing to do.
        match *self.current_scenario.borrow() {
            None => return Ok(()),
            Some(ref s) => {
                let current_scenario = s.borrow();
                if current_scenario.id() != id {
                    return Ok(());
                }
            }
        }
        if let Some(ref old_scenario) = self.current_scenario.borrow_mut().take() {
            old_scenario.borrow_mut().deselect()?;
        }
        Ok(())
    }

    pub fn activate(&self, id: &UnitName) {
        let result = match *id.kind() {
            UnitKind::Interface => self.activate_interface(id),
            UnitKind::Jig => self.activate_jig(id),
            UnitKind::Scenario => self.activate_scenario(id),
            UnitKind::Test => self.activate_test(id),
            UnitKind::Internal => Ok(()),
        };

        // Announce that the interface was successfully started.
        match result {
            Ok(_) => self.bc.broadcast(&UnitEvent::Status(UnitStatusEvent::new_active(id))),
            Err(e) =>
               self.bc.broadcast(
                    &UnitEvent::Status(UnitStatusEvent::new_active_failed(id, format!("unable to deactivate: {}", e)))),
        }
    }


    fn activate_interface(&self, id: &UnitName) -> Result<(), UnitActivateError> {
        let interface = match self.interfaces.borrow().get(id) {
            Some(i) => i.clone(),
            None => return Err(UnitActivateError::UnitNotFound),
        };

        // Activate the interface, which actually starts it up.
        interface.borrow_mut().activate(self, &*self.cfg.lock().unwrap())?;

        Ok(())
    }

    /// Set the new jig as "Active".
    /// If there is already an "Active" jig, then deactivate it.
    /// Only do so if there aren't any other valid, active jigs.
    fn activate_jig(&self, id: &UnitName) -> Result<(), UnitActivateError> {
        let new_jig = match self.jigs.borrow().get(id) {
            Some(s) => s.clone(),
            None => return Err(UnitActivateError::UnitNotFound),
        };

        // If there is an existing current jig, deactivate it.
        // There Can Only Be One.
        if let Some(ref old_jig) = self.current_jig.borrow_mut().take() {
            self.deactivate(old_jig.borrow().id(), "switching to a different jig");
        }

        // Activate this jig.
        new_jig.borrow_mut().activate()?;
        *self.current_jig.borrow_mut() = Some(new_jig.clone());
        self.bc
            .broadcast(&UnitEvent::Status(UnitStatusEvent::new_active(id)));

        // If there is a default scenario, activate that too.
        if let Some(new_scenario_id) = new_jig.borrow().default_scenario().clone() {
            self.activate_scenario(&new_scenario_id)?;
        }

        Ok(())
    }

    fn activate_scenario(&self, id: &UnitName) -> Result<(), UnitActivateError> {
        let new_scenario = match self.scenarios.borrow().get(id) {
            Some(s) => s.clone(),
            None => return Err(UnitActivateError::UnitNotFound),
        };

        // If there is an existing current scenario, deactivate it.
        // There Can Only Be One.
        if let Some(ref old_scenario) = self.current_scenario.borrow_mut().take() {
            self.deactivate(old_scenario.borrow().id(), "switching to a new scenario");
        }
        
        // Activate this scenario.
        new_scenario.borrow_mut().activate()?;
        *self.current_scenario.borrow_mut() = Some(new_scenario.clone());
        self.bc
            .broadcast(&UnitEvent::Status(UnitStatusEvent::new_active(id)));
        Ok(())
    }

    fn activate_test(&self, _id: &UnitName) -> Result<(), UnitActivateError> {
        unimplemented!();
    }

    pub fn deactivate(&self, id: &UnitName, reason: &str) {
        self.deselect(id, reason);
        let result = match *id.kind() {
            UnitKind::Interface => self.deactivate_interface(id),
            UnitKind::Jig => self.deactivate_jig(id),
            UnitKind::Scenario => self.deactivate_scenario(id),
            UnitKind::Test => self.deactivate_test(id),
            UnitKind::Internal => Ok(()),
        };
        match result {
            Ok(_) => self.bc.broadcast(&UnitEvent::Status(UnitStatusEvent::new_deactivate_success(id, reason.to_owned()))),
            Err(e) =>
                self.bc.broadcast(
                        &UnitEvent::Status(UnitStatusEvent::new_deactivate_failure(id, format!("unable to deactivate: {}", e)))),
        }
    }

    fn deactivate_interface(&self, id: &UnitName) -> Result<(), UnitDeactivateError> {
        let interfaces = self.interfaces.borrow();
        match interfaces.get(id) {
            None => return Err(UnitDeactivateError::UnitNotFound),
            Some(interface) => interface.borrow_mut().deactivate(),
        }
    }

    fn deactivate_test(&self, _id: &UnitName) -> Result<(), UnitDeactivateError> {
        unimplemented!();
    }

    fn deactivate_scenario(&self, id: &UnitName) -> Result<(), UnitDeactivateError> {
        let mut current_scenario_opt = self.current_scenario.borrow_mut();

        // If the specified scenario isn't the current scenario, then there's nothing to do.
        match *current_scenario_opt {
            None => return Ok(()),
            Some(ref s) => {
                let current_scenario = s.borrow();
                if current_scenario.id() != id {
                    return Ok(());
                }
            }
        }

        let scenario_rc = current_scenario_opt.take().unwrap();
        scenario_rc.borrow_mut().deactivate()?;
        Ok(())
    }

    fn deactivate_jig(&self, id: &UnitName) -> Result<(), UnitDeactivateError> {
        Ok(())
    }

    pub fn unload(&self, id: &UnitName) {
        self.deselect(id, "unloading");
        match *id.kind() {
            UnitKind::Interface => self.unload_interface(id),
            UnitKind::Jig => self.unload_jig(id),
            UnitKind::Scenario => self.unload_scenario(id),
            UnitKind::Test => self.unload_test(id),
            UnitKind::Internal => (),
        }
    }
    
    fn unload_interface(&self, id: &UnitName) {
        self.interfaces.borrow_mut().remove(id);
    }

    fn unload_jig(&self, id: &UnitName) {
        self.jigs.borrow_mut().remove(id);
    }

    fn unload_test(&self, id: &UnitName) {
        self.tests.borrow_mut().remove(id);
    }

    fn unload_scenario(&self, id: &UnitName) {
        self.scenarios.borrow_mut().remove(id);
    }

    pub fn get_scenario_named(&self, id: &UnitName) -> Option<Rc<RefCell<Scenario>>> {
        match self.scenarios.borrow().get(id) {
            None => None,
            Some(scenario) => Some(scenario.clone())
        }
    }

    pub fn get_test_named(&self, id: &UnitName) -> Option<Rc<RefCell<Test>>> {
        match self.tests.borrow().get(id) {
            None => None,
            Some(test) => Some(test.clone()),
        }
    }

    pub fn get_tests(&self) -> Rc<RefCell<HashMap<UnitName, Rc<RefCell<Test>>>>> {
        self.tests.clone()
    }

    pub fn get_scenarios(&self) -> Rc<RefCell<HashMap<UnitName, Rc<RefCell<Scenario>>>>> {
        self.scenarios.clone()
    }

     pub fn jig_is_loaded(&self, id: &UnitName) -> bool {
        self.jigs.borrow().get(id).is_some()
    }

    pub fn process_message(&self, msg: &UnitEvent) {
        match msg {
            &UnitEvent::ManagerRequest(ref req) => self.manager_request(req),
            &UnitEvent::Status(ref stat) => self.status_message(stat),
            &UnitEvent::Log(ref log) => {
                for (_, interface) in self.interfaces.borrow().iter() {
                    let log_status_msg = ManagerStatusMessage::Log(log.clone());
                    interface.borrow().output_message(log_status_msg).expect("Unable to pass message to client");
                }
            },
            _ => (),
        }
    }

    fn status_message(&self, msg: &UnitStatusEvent) {
        let &UnitStatusEvent {ref name, ref status} = msg;
        match status {
            &UnitStatus::Active => match name.kind() {
                &UnitKind::Jig => self.broadcast_jig_named(name),
                &UnitKind::Scenario => self.broadcast_scenario_named(name),
                _ => (),
            },
            _ => (),
        }
    }

    fn manager_request(&self, msg: &ManagerControlMessage) {
        let &ManagerControlMessage {sender: ref sender_name, contents: ref msg} = msg;

        match *msg {
            ManagerControlMessageContents::Scenarios => self.send_scenarios_to(sender_name),
            ManagerControlMessageContents::Tests(ref scenario_name) => self.send_tests_to(sender_name, scenario_name),
            ManagerControlMessageContents::Log(ref txt) => self.bc.broadcast(&UnitEvent::Log(LogEntry::new_info(sender_name.clone(), txt.clone()))),
            ManagerControlMessageContents::LogError(ref txt) => self.bc.broadcast(&UnitEvent::Log(LogEntry::new_error(sender_name.clone(), txt.clone()))),
            ManagerControlMessageContents::Scenario(ref new_scenario_name) => {
                if self.get_scenario_named(new_scenario_name).is_some() {
                    self.activate(new_scenario_name);
                } else {
                    self.bc.broadcast(&UnitEvent::Log(LogEntry::new_error(sender_name.clone(), format!("unable to find scenario {}", new_scenario_name))));
                }
            },
            ManagerControlMessageContents::Error(ref err) => {
                self.bc.broadcast(&UnitEvent::Log(LogEntry::new_error(sender_name.clone(), err.clone())));
            },
            ManagerControlMessageContents::Jig => self.send_jig_to(sender_name),
            ManagerControlMessageContents::InitialGreeting => {
                // Send some initial information to the client.
                self.send_hello_to(sender_name);
                self.send_jig_to(sender_name);
                self.send_scenarios_to(sender_name);
                // If there is a scenario selected, send that too.
                if let Some(ref sc) = *self.current_scenario.borrow() {
                    self.send_scenario_to(sender_name, &sc.borrow().id().clone());
                }
            },
            ManagerControlMessageContents::ChildExited => {
                self.bc.broadcast(&UnitEvent::Status(UnitStatusEvent::new_active_failed(sender_name, "Unit unexpectedly exited".to_owned())));
            }
            ManagerControlMessageContents::Unimplemented(ref verb, ref remainder) => {
                self.bc.broadcast(&UnitEvent::Log(LogEntry::new_error(sender_name.clone(), format!("unimplemented verb: {} (args: {})", verb, remainder))));
            },
            ManagerControlMessageContents::Start(ref scenario_name_opt) => {
                let scenario_rc = if let Some(ref scenario_name) = *scenario_name_opt {
                    match self.scenarios.borrow().get(scenario_name) {
                        None => {
                            self.bc.broadcast(&UnitEvent::Log(LogEntry::new_error(sender_name.clone(), format!("unable to find scenario {} to start it", scenario_name))));
                            return;
                        },
                        Some(s) => s.clone(),
                    }
                } else {
                    match *self.current_scenario.borrow() {
                        None => {
                            self.bc.broadcast(&UnitEvent::Log(LogEntry::new_error(sender_name.clone(), "no scenario selected to start".to_owned())));
                            return;
                        },
                        Some(ref s) => s.clone(),
                    }
                };
            }
        }
    }

    pub fn send_hello_to(&self, sender_name: &UnitName) {
        self.send_messages_to(sender_name, vec![ManagerStatusMessage::Hello("Jig/20 1.0".to_owned())]);
    }

    pub fn send_jig_to(&self, sender_name: &UnitName) {
        let messages = match *self.current_jig.borrow() {
            None => vec![ManagerStatusMessage::Jig(UnitName::from_str("", "jig").unwrap())],
            Some(ref jig_rc) => {
                let jig = jig_rc.borrow();
                vec![
                    ManagerStatusMessage::Jig(jig.id().clone()),
                    ManagerStatusMessage::Describe(jig.id().kind().clone(), FieldType::Name, jig.id().id().clone(), jig.name().clone()),
                    ManagerStatusMessage::Describe(jig.id().kind().clone(), FieldType::Description, jig.id().id().clone(), jig.description().clone())
                ]
            }
        };
        self.send_messages_to(sender_name, messages);
    }

    /// Send all available scenarios to the specified endpoint.
    pub fn send_scenarios_to(&self, sender_name: &UnitName) {
        let mut messages = vec![ManagerStatusMessage::Scenarios(self.scenarios.borrow().keys().map(|x| x.clone()).collect())];
        for (scenario_id, scenario) in self.scenarios.borrow().iter() {
            messages.push(ManagerStatusMessage::Describe(scenario_id.kind().clone(), FieldType::Name, scenario_id.id().clone(), scenario.borrow().name().clone()));
            messages.push(ManagerStatusMessage::Describe(scenario_id.kind().clone(), FieldType::Description, scenario_id.id().clone(), scenario.borrow().description().clone()));
        }
        self.send_messages_to(sender_name, messages);
    }

    pub fn send_scenario_to(&self, sender_name: &UnitName, scenario_name: &UnitName) {
        let messages = match self.scenarios.borrow().get(scenario_name) {
            None => vec![ManagerStatusMessage::Scenario(None)],
            Some(scenario_rc) => {
                let scenario = scenario_rc.borrow();
                let mut messages = vec![ManagerStatusMessage::Scenario(Some(scenario_name.clone()))];
                for (test_id, test_rc) in scenario.tests() {
                    let test = test_rc.borrow();
                    messages.push(ManagerStatusMessage::Describe(test_id.kind().clone(), FieldType::Name, test_id.id().clone(), test.name().clone()));
                    messages.push(ManagerStatusMessage::Describe(test_id.kind().clone(), FieldType::Description, test_id.id().clone(), test.description().clone()));
                }
                messages.push(ManagerStatusMessage::Tests(scenario.id().clone(), scenario.test_sequence()));
                messages
            }
        };
        self.send_messages_to(sender_name, messages);
    }

    /// Send a list of tests to the specified recipient.
    /// If no scenario name is specified, send the current scenario.
    pub fn send_tests_to(&self, sender_name: &UnitName, scenario_name_opt: &Option<UnitName>) {
        let scenario_id = match *scenario_name_opt {
            Some(ref n) => n.clone(),
            None => match *self.current_scenario.borrow() {
                Some(ref cs) => cs.borrow().id().clone(),
                None => {
                    self.bc.broadcast(&UnitEvent::Log(LogEntry::new_error(sender_name.clone(), "unable to list tests, no scenario specified and no scenario selected".to_owned())));
                    return;
                }
            }
        };
        let scenarios = self.scenarios.borrow();
        let scenario_rc_opt = scenarios.get(&scenario_id);
        match scenario_rc_opt {
            None => self.bc.broadcast(&UnitEvent::Log(LogEntry::new_error(sender_name.clone(), format!("unable to list tests, scenario {} not found", scenario_id)))),
            Some(ref sc_ref) => {
                let scenario = sc_ref.borrow();
                self.send_messages_to(sender_name, vec![ManagerStatusMessage::Tests(scenario.id().clone(), scenario.test_sequence())])
            }
        }
    }

    fn broadcast_jig_named(&self, jig_id: &UnitName) {
        let jigs = self.jigs.borrow();
        let jig = match jigs.get(jig_id) {
            Some(ref s) => s.clone(),
            None => return,
        };
        for (interface_id, _) in self.interfaces.borrow().iter() {
            let jig = jig.borrow();
            let messages = vec![
                ManagerStatusMessage::Jig(jig.id().clone()),
                ManagerStatusMessage::Describe(jig.id().kind().clone(), FieldType::Name, jig.id().id().clone(), jig.name().clone()),
                ManagerStatusMessage::Describe(jig.id().kind().clone(), FieldType::Description, jig.id().id().clone(), jig.description().clone())
            ];
            self.send_messages_to(interface_id, messages);
        }
    }

    fn broadcast_scenario_named(&self, scenario_id: &UnitName) {
        let scenarios = self.scenarios.borrow();
        let scenario = match scenarios.get(scenario_id) {
            Some(ref s) => s.clone(),
            None => return,
        };
        for (interface_id, _) in self.interfaces.borrow().iter() {
            let scenario = scenario.borrow();
            let messages = vec![
                ManagerStatusMessage::Scenario(Some(scenario.id().clone())),
                ManagerStatusMessage::Describe(scenario.id().kind().clone(), FieldType::Name, scenario.id().id().clone(), scenario.name().clone()),
                ManagerStatusMessage::Describe(scenario.id().kind().clone(), FieldType::Description, scenario.id().id().clone(), scenario.description().clone())
            ];
            self.send_messages_to(interface_id, messages);
        }
    }

    /// Send a Vec<ManagerStatusMessage> to a specific endpoint.
    pub fn send_messages_to(&self, sender_name: &UnitName, messages: Vec<ManagerStatusMessage>) {
        let mut deactivate_reason = None;
        match *sender_name.kind() {
            UnitKind::Interface => {
                let interface_table = self.interfaces.borrow();
                let interface = interface_table.get(sender_name).expect("Unable to find Interface in the library");
                for msg in messages {
                    if let Err(e) = interface.borrow().output_message(msg) {
                        deactivate_reason = Some(e);
                        break;
                    }
                }
            },
            _ => (),
        }
        if let Some(deactivate_reason) = deactivate_reason {
            self.deactivate(sender_name, format!("communication error: {}", deactivate_reason).as_str());
        }
    }
}