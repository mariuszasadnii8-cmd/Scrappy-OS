#![no_std]
#![feature(alloc_error_handler)]

extern crate alloc;

use alloc::vec::Vec;
use alloc::string::String;
use alloc::boxed::Box;
use core::fmt;

// ============================================================================
// TYPES (types.rs)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellId(pub u8);

impl CellId {
    pub const EMERGENCY: Self = CellId(6);
    pub const MAIN: Self = CellId(0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellStatus {
    Uninitialized,
    Loading,
    Running,
    Crashed,
    Stopped,
    Restoring,
}

#[derive(Debug, Clone)]
pub enum WasmError {
    LoadError(String),
    RuntimeError(String),
    Timeout,
    MemoryError,
}

impl fmt::Display for WasmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WasmError::LoadError(s) => write!(f, "LoadError: {}", s),
            WasmError::RuntimeError(s) => write!(f, "RuntimeError: {}", s),
            WasmError::Timeout => write!(f, "Timeout"),
            WasmError::MemoryError => write!(f, "MemoryError"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmailMessage {
    pub from: CellId,
    pub to: CellId,
    pub subject: String,
    pub body: String,
}

impl EmailMessage {
    pub fn new(from: CellId, to: CellId, subject: &str, body: &str) -> Self {
        Self {
            from,
            to,
            subject: String::from(subject),
            body: String::from(body),
        }
    }
}

// ============================================================================
// RUNNER (runner.rs) - Упрощенная версия для демонстрации
// ============================================================================

pub struct ModuleInstance {
    pub memory: Vec<u8>,
    pub globals: Vec<i64>,
    pub status: CellStatus,
}

impl ModuleInstance {
    pub fn new() -> Self {
        Self {
            memory: vec![0; 1024], // 1KB памяти для примера
            globals: vec![0; 16],
            status: CellStatus::Uninitialized,
        }
    }
}

pub struct ScrappyWasmRunner {
    cells: [Option<ModuleInstance>; 7],
    pub mailbox: KernelMailbox,
}

impl ScrappyWasmRunner {
    pub fn new() -> Self {
        Self {
            cells: [None, None, None, None, None, None, None],
            mailbox: KernelMailbox::new(),
        }
    }

    pub fn load(&mut self, wasm_bytes: &[u8], cell_id: CellId, _wasi_ctx: ()) -> Result<(), WasmError> {
        // В реальной системе здесь была бы загрузка WASM модуля
        if wasm_bytes.is_empty() {
            return Err(WasmError::LoadError("Empty WASM".to_string()));
        }
        
        let mut instance = ModuleInstance::new();
        // Копируем заголовок WASM в память для эмуляции
        for (i, &byte) in wasm_bytes.iter().enumerate() {
            if i < instance.memory.len() {
                instance.memory[i] = byte;
            }
        }
        instance.status = CellStatus::Loading;
        self.cells[cell_id.0 as usize] = Some(instance);
        Ok(())
    }

    pub fn start(&mut self, cell_id: CellId) -> Result<(), WasmError> {
        if let Some(cell) = &mut self.cells[cell_id.0 as usize] {
            cell.status = CellStatus::Running;
            Ok(())
        } else {
            Err(WasmError::RuntimeError("Cell not loaded".to_string()))
        }
    }

    pub fn get_status(&self, cell_id: CellId) -> CellStatus {
        self.cells[cell_id.0 as usize]
            .as_ref()
            .map(|c| c.status)
            .unwrap_or(CellStatus::Uninitialized)
    }

    pub fn execute_quantum(&mut self, cell_id: CellId, _instructions: u64) -> Result<(), WasmError> {
        if let Some(cell) = &mut self.cells[cell_id.0 as usize] {
            if cell.status != CellStatus::Running {
                return Err(WasmError::RuntimeError("Cell not running".to_string()));
            }
            // Эмуляция выполнения - в реальности здесь был бы интерпретатор WASM
            Ok(())
        } else {
            Err(WasmError::RuntimeError("Cell not found".to_string()))
        }
    }

    pub fn stop_cell(&mut self, cell_id: CellId) {
        if let Some(cell) = &mut self.cells[cell_id.0 as usize] {
            cell.status = CellStatus::Stopped;
        }
    }

    pub fn get_instance(&mut self, cell_id: CellId) -> Option<&mut ModuleInstance> {
        self.cells[cell_id.0 as usize].as_mut()
    }

    pub fn set_instance(&mut self, cell_id: CellId, instance: ModuleInstance) {
        self.cells[cell_id.0 as usize] = Some(instance);
    }
}

impl Default for ScrappyWasmRunner {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// MAILBOX SYSTEM
// ============================================================================

pub struct KernelMailbox {
    messages: Vec<EmailMessage>,
}

impl KernelMailbox {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    pub fn send(&mut self, msg: EmailMessage) {
        log_info(&format!("MAIL: {} -> {}: [{}] {}", 
            msg.from.0, msg.to.0, msg.subject, msg.body));
        self.messages.push(msg);
    }

    pub fn receive_for(&mut self, cell_id: CellId) -> Vec<EmailMessage> {
        let mut result = Vec::new();
        let mut i = 0;
        while i < self.messages.len() {
            if self.messages[i].to == cell_id {
                result.push(self.messages.remove(i));
            } else {
                i += 1;
            }
        }
        result
    }

    pub fn process_pending_commands(&mut self, runner: &mut ScrappyWasmRunner) {
        // Обработка команд от Cell 6
        let commands = self.receive_for(CellId(6));
        for cmd in commands {
            log_info(&format!("Cell 6 processing command: {}", cmd.body));
            // Здесь логика выполнения команд восстановления
        }
    }
}

impl Default for KernelMailbox {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// SCRAPBOOT MODULE
// ============================================================================

// Вшивание WASM файлов
const INIT_WASM: &[u8] = include_bytes!("../wasm_bins/init.wasm");
const EMERGENCY_WASM: &[u8] = include_bytes!("../wasm_bins/emergency.wasm");

/// Снимок состояния клетки для восстановления
#[derive(Debug, Clone)]
pub struct CellSnapshot {
    pub cell_id: CellId,
    pub memory_state: Vec<u8>,
    pub global_state: Vec<i64>,
    pub last_status: CellStatus,
    pub timestamp: u64,
}

impl CellSnapshot {
    pub fn take(cell_id: CellId, instance: &ModuleInstance) -> Self {
        Self {
            cell_id,
            memory_state: instance.memory.clone(),
            global_state: instance.globals.clone(),
            last_status: instance.status,
            timestamp: 0, // В реальной системе здесь было бы время
        }
    }

    pub fn restore(&self, instance: &mut ModuleInstance) -> Result<(), WasmError> {
        instance.memory = self.memory_state.clone();
        instance.globals = self.global_state.clone();
        instance.status = CellStatus::Running;
        Ok(())
    }
}

/// Загрузка и инициализация системы
pub fn bootstrap(runner: &mut ScrappyWasmRunner, mailbox: &mut KernelMailbox) -> Result<(), WasmError> {
    log_info("=== ScrapBoot: Starting System Initialization ===");

    // ШАГ 1: Загрузка Emergency Core (Cell 6) - КРИТИЧНО
    log_info("ScrapBoot: Loading Cell 6 (Emergency Core)...");
    runner.load(EMERGENCY_WASM, CellId::EMERGENCY, ())?;
    
    log_info("ScrapBoot: Starting Cell 6...");
    runner.start(CellId::EMERGENCY)?;
    
    // Проверка статуса Cell 6
    if runner.get_status(CellId::EMERGENCY) != CellStatus::Running {
        return Err(WasmError::RuntimeError("Emergency Core failed to start".to_string()));
    }
    
    log_info("ScrapBoot: Cell 6 is RUNNING - Emergency Core active");

    // ШАГ 2: Только если Cell 6 работает, загружаем Main Shell (Cell 0)
    log_info("ScrapBoot: Loading Cell 0 (Main Shell)...");
    runner.load(INIT_WASM, CellId::MAIN, ())?;
    
    log_info("ScrapBoot: Starting Cell 0...");
    runner.start(CellId::MAIN)?;
    
    if runner.get_status(CellId::MAIN) != CellStatus::Running {
        // Отправляем отчет об ошибке в Cell 6
        let error_msg = EmailMessage::new(
            CellId(255), // Системный ID
            CellId::EMERGENCY,
            "BOOT_FAILURE",
            "Main Shell failed to start"
        );
        mailbox.send(error_msg);
        return Err(WasmError::RuntimeError("Main Shell failed to start".to_string()));
    }
    
    log_info("ScrapBoot: Cell 0 is RUNNING - Main Shell active");
    log_info("=== ScrapBoot: System Initialization Complete ===");
    
    Ok(())
}

/// Создание снапшота состояния клетки
pub fn snapshot_take(cell_id: CellId, runner: &mut ScrappyWasmRunner) -> Result<CellSnapshot, WasmError> {
    log_info(&format!("Creating snapshot for Cell {}", cell_id.0));
    
    if let Some(instance) = runner.get_instance(cell_id) {
        let snapshot = CellSnapshot::take(cell_id, instance);
        log_info(&format!("Snapshot created: {} bytes memory, {} globals", 
            snapshot.memory_state.len(), snapshot.global_state.len()));
        Ok(snapshot)
    } else {
        Err(WasmError::MemoryError)
    }
}

/// Восстановление клетки из снапшота
pub fn snapshot_restore(cell_id: CellId, snapshot: &CellSnapshot, runner: &mut ScrappyWasmRunner) -> Result<(), WasmError> {
    log_info(&format!("Restoring Cell {} from snapshot...", cell_id.0));
    
    if let Some(instance) = runner.get_instance(cell_id) {
        snapshot.restore(instance)?;
        log_info(&format!("Cell {} restored successfully", cell_id.0));
        Ok(())
    } else {
        let mut new_instance = ModuleInstance::new();
        snapshot.restore(&mut new_instance)?;
        runner.set_instance(cell_id, new_instance);
        log_info(&format!("Cell {} restored with new instance", cell_id.0));
        Ok(())
    }
}

/// Обработка ошибки клетки с отправкой отчета в Cell 6
pub fn handle_cell_error(
    cell_id: CellId, 
    error: &WasmError, 
    runner: &mut ScrappyWasmRunner,
    mailbox: &mut KernelMailbox
) {
    log_error(&format!("Cell {} crashed: {}", cell_id.0, error));
    
    // Попытка создать снапшот упавшей клетки
    let snapshot_result = snapshot_take(cell_id, runner);
    
    // Формирование отчета об ошибке
    let error_code = match error {
        WasmError::LoadError(s) => format!("LOAD_ERR: {}", s),
        WasmError::RuntimeError(s) => format!("RUN_ERR: {}", s),
        WasmError::Timeout => "TIMEOUT".to_string(),
        WasmError::MemoryError => "MEM_ERR".to_string(),
    };
    
    let report_body = format!("CRASH_REPORT\nCell: {}\nError: {}\nSnapshot: {}", 
        cell_id.0, 
        error_code,
        if snapshot_result.is_ok() { "SAVED" } else { "FAILED" }
    );
    
    // Отправка EmailMessage в Cell 6
    let crash_report = EmailMessage::new(
        cell_id,
        CellId::EMERGENCY,
        "SYSTEM_CRASH",
        &report_body
    );
    
    mailbox.send(crash_report);
    log_info("Crash report sent to Emergency Core (Cell 6)");
    
    // Остановка упавшей клетки
    runner.stop_cell(cell_id);
}

// ============================================================================
// HEALTH MONITOR (Watchdog / "Судный день")
// ============================================================================

const CELL_TIMEOUT_MS: u64 = 5000;

pub struct HealthMonitor {
    last_heartbeat: [u64; 7],
    snapshots: [Option<CellSnapshot>; 7],
}

impl HealthMonitor {
    pub fn new() -> Self {
        Self {
            last_heartbeat: [0; 7],
            snapshots: [None, None, None, None, None, None, None],
        }
    }

    /// Запись хартбита от клетки
    pub fn record_heartbeat(&mut self, cell_id: u8, current_time: u64) {
        if cell_id <= 6 {
            self.last_heartbeat[cell_id as usize] = current_time;
        }
    }

    /// Главный цикл надзирателя
    pub fn tick(&mut self, runner: &mut ScrappyWasmRunner, current_time_ms: u64, mailbox: &mut KernelMailbox) {
        for id in 0..=6 {
            let cell_id = CellId(id as u8);
            
            // Пропускаем Cell 6 (надзиратель) и не запущенные клетки
            if id == 6 {
                continue;
            }
            
            let status = runner.get_status(cell_id);
            if status != CellStatus::Running {
                continue;
            }

            let time_since_heartbeat = current_time_ms.saturating_sub(self.last_heartbeat[id]);

            if time_since_heartbeat > CELL_TIMEOUT_MS {
                log_error(&format!("WATCHDOG: Cell {} timeout ({}ms > {}ms)", 
                    id, time_since_heartbeat, CELL_TIMEOUT_MS));
                
                // Создаем снапшот если еще нет
                if self.snapshots[id].is_none() {
                    if let Ok(snapshot) = snapshot_take(cell_id, runner) {
                        self.snapshots[id] = Some(snapshot);
                        log_info(&format!("WATCHDOG: Snapshot saved for Cell {}", id));
                    }
                }

                // Отправляем алерт в Cell 6
                let alert = EmailMessage::new(
                    cell_id,
                    CellId::EMERGENCY,
                    "WATCHDOG_ALERT",
                    &format!("CRASH_REPORT\nCell: {}\nReason: WATCHDOG_TIMEOUT", id)
                );
                mailbox.send(alert);

                // Принудительное восстановление
                log_warn(&format!("WATCHDOG: Initiating forced restore for Cell {}", id));
                
                if let Some(snapshot) = &self.snapshots[id] {
                    match snapshot_restore(cell_id, snapshot, runner) {
                        Ok(_) => {
                            self.last_heartbeat[id] = current_time_ms;
                            log_info(&format!("WATCHDOG: Cell {} restored successfully", id));
                        }
                        Err(e) => {
                            log_error(&format!("WATCHDOG: Restore failed for Cell {}: {}", id, e));
                            runner.stop_cell(cell_id);
                        }
                    }
                } else {
                    log_error(&format!("WATCHDOG: No snapshot available for Cell {}, halting", id));
                    runner.stop_cell(cell_id);
                }
            }
        }
    }
}

impl Default for HealthMonitor {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// LOGGING UTILITIES
// ============================================================================

fn log_info(msg: &str) {
    // В реальной системе: kernel_log!(INFO, "{}", msg);
    // Для эмуляции можно использовать println! если доступен
    #[cfg(feature = "std")]
    println!("[INFO] {}", msg);
}

fn log_error(msg: &str) {
    #[cfg(feature = "std")]
    println!("[ERROR] {}", msg);
}

fn log_warn(msg: &str) {
    #[cfg(feature = "std")]
    println!("[WARN] {}", msg);
}

// ============================================================================
// MAIN INTEGRATION EXAMPLE
// ============================================================================

/// Пример главного цикла ОС
pub fn run_os_loop() -> ! {
    let mut runner = ScrappyWasmRunner::new();
    let mut mailbox = KernelMailbox::new();
    let mut monitor = HealthMonitor::new();
    
    // Инициализация через ScrapBoot
    if let Err(e) = bootstrap(&mut runner, &mut mailbox) {
        panic!("FATAL BOOT ERROR: {:?}", e);
    }

    let mut system_time: u64 = 0;

    loop {
        system_time = system_time.saturating_add(1); // 1ms тик

        // Выполнение квантов для всех клеток
        for id in 0..=6 {
            let cell_id = CellId(id as u8);
            
            if runner.get_status(cell_id) == CellStatus::Running {
                match runner.execute_quantum(cell_id, 1000) {
                    Ok(_) => {
                        monitor.record_heartbeat(id, system_time);
                    }
                    Err(e) => {
                        handle_cell_error(cell_id, &e, &mut runner, &mut mailbox);
                    }
                }
            }
        }

        // Проверка здоровья (Watchdog)
        monitor.tick(&mut runner, system_time, &mut mailbox);

        // Обработка команд почты
        mailbox.process_pending_commands(&mut runner);

        // В реальной системе: cpu::halt() или WFI
        #[cfg(feature = "std")]
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bootstrap_sequence() {
        let mut runner = ScrappyWasmRunner::new();
        let mut mailbox = KernelMailbox::new();
        
        // Bootstrap должен запустить Cell 6 перед Cell 0
        assert!(bootstrap(&mut runner, &mut mailbox).is_ok());
        assert_eq!(runner.get_status(CellId::EMERGENCY), CellStatus::Running);
        assert_eq!(runner.get_status(CellId::MAIN), CellStatus::Running);
    }

    #[test]
    fn test_snapshot_system() {
        let mut runner = ScrappyWasmRunner::new();
        let mut mailbox = KernelMailbox::new();
        
        bootstrap(&mut runner, &mut mailbox).unwrap();
        
        // Создаем снапшот
        let snapshot = snapshot_take(CellId::MAIN, &mut runner).unwrap();
        assert_eq!(snapshot.cell_id, CellId::MAIN);
        
        // Модифицируем память
        if let Some(instance) = runner.get_instance(CellId::MAIN) {
            instance.memory[0] = 0x42;
            instance.globals[0] = 12345;
        }
        
        // Восстанавливаем из снапшота
        snapshot_restore(CellId::MAIN, &snapshot, &mut runner).unwrap();
        
        // Проверяем восстановление
        if let Some(instance) = runner.get_instance(CellId::MAIN) {
            assert_eq!(instance.memory[0], 0); // Должно быть как в снапшоте
            assert_eq!(instance.globals[0], 0);
        }
    }

    #[test]
    fn test_error_handling() {
        let mut runner = ScrappyWasmRunner::new();
        let mut mailbox = KernelMailbox::new();
        
        bootstrap(&mut runner, &mut mailbox).unwrap();
        
        // Симулируем ошибку
        let error = WasmError::RuntimeError("Test error".to_string());
        handle_cell_error(CellId::MAIN, &error, &mut runner, &mut mailbox);
        
        // Проверяем что клетка остановлена
        assert_eq!(runner.get_status(CellId::MAIN), CellStatus::Stopped);
        
        // Проверяем что сообщение отправлено в Cell 6
        let messages = mailbox.receive_for(CellId::EMERGENCY);
        assert!(!messages.is_empty());
        assert!(messages[0].body.contains("CRASH_REPORT"));
    }
}
