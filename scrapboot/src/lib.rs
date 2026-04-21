#![no_std]
#![feature(alloc_error_handler)]
#![feature(core_intrinsics)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::NonNull;

// ============================================================================
// Constants
// ============================================================================

/// Количество ядер (клеток) в системе
pub const CELL_COUNT: usize = 7;

/// ID ядра-спасателя (инициализируется первым)
pub const RESCUE_CELL_ID: usize = 6;

/// ID главного ядра
pub const MAIN_CELL_ID: usize = 0;

// ============================================================================
// Type Definitions
// ============================================================================

/// Статус клетки
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellStatus {
    /// Клетка не инициализирована
    Uninitialized,
    /// Клетка загружается
    Loading,
    /// Клетка работает нормально
    Running,
    /// Клетка упала с ошибкой
    Crashed,
    /// Клетка остановлена
    Stopped,
}

/// Типы ошибок WASM
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WasmError {
    /// Ошибка компиляции модуля
    CompileError(String),
    /// Ошибка инстанциации
    InstantiationError(String),
    /// Ошибка выполнения (trap)
    Trap(String),
    /// Ошибка хоста при вызове функции
    HostError(String),
}

/// Сообщение электронной почты для межклеточной коммуникации
#[derive(Debug, Clone)]
pub struct EmailMessage {
    /// Отправитель (ID клетки)
    pub from: usize,
    /// Получатель (ID клетки)
    pub to: usize,
    /// Тема сообщения
    pub subject: String,
    /// Тело сообщения
    pub body: String,
    /// Приоритет (true = высокий)
    pub high_priority: bool,
}

impl EmailMessage {
    pub fn new(from: usize, to: usize, subject: &str, body: &str) -> Self {
        Self {
            from,
            to,
            subject: String::from(subject),
            body: String::from(body),
            high_priority: false,
        }
    }

    pub fn high_priority(mut self) -> Self {
        self.high_priority = true;
        self
    }
}

/// Снимок состояния клетки
#[derive(Debug, Clone)]
pub struct CellSnapshot {
    /// ID клетки
    pub cell_id: usize,
    /// Сериализованное состояние памяти
    pub memory_state: Vec<u8>,
    /// Сериализованное состояние регистров/контекста
    pub register_state: Vec<u8>,
    /// Последний известный статус
    pub last_status: CellStatus,
    /// Описание ошибки (если была)
    pub error_info: Option<String>,
}

/// Абстрактный интерфейс для WASM модуля
/// В реальной реализации здесь был бы wasmtime/wasm3/etc.
pub trait WasmModule: Send + Sync {
    /// Инициализировать модуль с данными
    fn init(&mut self, wasm_data: &[u8]) -> Result<(), WasmError>;
    
    /// Запустить точку входа
    fn run(&mut self) -> Result<(), WasmError>;
    
    /// Получить снимок состояния
    fn take_snapshot(&self) -> Vec<u8>;
    
    /// Восстановить из снимка
    fn restore_from_snapshot(&mut self, snapshot: &[u8]) -> Result<(), WasmError>;
}

/// Пример простой реализации (заглушка для демонстрации)
pub struct SimpleWasmModule {
    data: Vec<u8>,
    is_initialized: bool,
}

impl SimpleWasmModule {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            is_initialized: false,
        }
    }
}

impl WasmModule for SimpleWasmModule {
    fn init(&mut self, wasm_data: &[u8]) -> Result<(), WasmError> {
        self.data = wasm_data.to_vec();
        self.is_initialized = true;
        Ok(())
    }

    fn run(&mut self) -> Result<(), WasmError> {
        if !self.is_initialized {
            return Err(WasmError::InstantiationError(String::from("Module not initialized")));
        }
        // Симуляция выполнения
        Ok(())
    }

    fn take_snapshot(&self) -> Vec<u8> {
        self.data.clone()
    }

    fn restore_from_snapshot(&mut self, snapshot: &[u8]) -> Result<(), WasmError> {
        self.data = snapshot.to_vec();
        self.is_initialized = !self.data.is_empty();
        Ok(())
    }
}

/// Экземпляр модуля клетки
pub struct ModuleInstance {
    /// Уникальный идентификатор
    pub cell_id: usize,
    /// Статус выполнения
    pub status: CellStatus,
    /// WASM модуль
    pub module: Box<dyn WasmModule>,
    /// Данные памяти
    pub memory: Vec<u8>,
}

impl ModuleInstance {
    pub fn new(cell_id: usize, module: Box<dyn WasmModule>) -> Self {
        Self {
            cell_id,
            status: CellStatus::Uninitialized,
            module,
            memory: Vec::new(),
        }
    }

    pub fn initialize(&mut self, wasm_data: &[u8]) -> Result<(), WasmError> {
        self.status = CellStatus::Loading;
        self.module.init(wasm_data)?;
        self.status = CellStatus::Running;
        Ok(())
    }

    pub fn execute(&mut self) -> Result<(), WasmError> {
        if self.status != CellStatus::Running {
            return Err(WasmError::InstantiationError(
                String::from("Cell not in Running state")
            ));
        }
        self.module.run()
    }

    pub fn crash(&mut self, error: WasmError) {
        self.status = CellStatus::Crashed;
        // Сохраняем информацию об ошибке в памяти для отладки
        let error_msg = format!("CRASH: {:?}", error);
        self.memory = error_msg.as_bytes().to_vec();
    }
}

// ============================================================================
// ScrapBoot Manager
// ============================================================================

/// Менеджер загрузки и управления клетками
pub struct ScrapBootManager {
    /// Массив клеток (фиксированный размер 7)
    cells: [Option<ModuleInstance>; CELL_COUNT],
    /// Очередь почтовых сообщений
    mail_queue: Vec<EmailMessage>,
    /// Хранилище снимков состояний
    snapshots: BTreeMap<usize, CellSnapshot>,
}

impl ScrapBootManager {
    /// Создать новый менеджер
    pub const fn new() -> Self {
        // Инициализация массива через unsafe для no_std совместимости
        // В реальном коде используйте MaybeUninit
        unsafe {
            Self {
                cells: core::mem::transmute([0u8; CELL_COUNT * core::mem::size_of::<Option<ModuleInstance>>()]),
                mail_queue: Vec::new(),
                snapshots: BTreeMap::new(),
            }
        }
    }

    /// Инициализировать менеджера (должен вызываться после new)
    pub fn init(&mut self) {
        for i in 0..CELL_COUNT {
            self.cells[i] = None;
        }
    }

    /// Взять снимок состояния упавшей клетки
    pub fn snapshot_take(&mut self, cell_id: usize) -> Option<CellSnapshot> {
        if cell_id >= CELL_COUNT {
            return None;
        }

        if let Some(cell) = &self.cells[cell_id] {
            if cell.status == CellStatus::Crashed {
                let snapshot = CellSnapshot {
                    cell_id,
                    memory_state: cell.memory.clone(),
                    register_state: cell.module.take_snapshot(),
                    last_status: cell.status,
                    error_info: Some(String::from_utf8_lossy(&cell.memory).to_string()),
                };
                
                self.snapshots.insert(cell_id, snapshot.clone());
                return Some(snapshot);
            }
        }
        
        None
    }

    /// Восстановить клетку из снимка
    pub fn snapshot_restore(&mut self, cell_id: usize) -> Result<(), WasmError> {
        if let Some(snapshot) = self.snapshots.remove(&cell_id) {
            if let Some(ref mut cell) = self.cells[cell_id] {
                cell.module.restore_from_snapshot(&snapshot.register_state)?;
                cell.memory = snapshot.memory_state;
                cell.status = CellStatus::Stopped; // Готов к перезапуску
                return Ok(());
            }
        }
        Err(WasmError::InstantiationError(String::from("No snapshot found")))
    }

    /// Отправить почтовое сообщение
    pub fn send_email(&mut self, message: EmailMessage) {
        self.mail_queue.push(message);
    }

    /// Получить очередь сообщений
    pub fn get_mail_queue(&self) -> &[EmailMessage] {
        &self.mail_queue
    }

    /// Очистить обработанные сообщения
    pub fn clear_processed_mail(&mut self, count: usize) {
        if count <= self.mail_queue.len() {
            for _ in 0..count {
                self.mail_queue.remove(0);
            }
        }
    }

    /// Проверить статус клетки
    pub fn get_cell_status(&self, cell_id: usize) -> Option<CellStatus> {
        if cell_id < CELL_COUNT {
            self.cells[cell_id].as_ref().map(|c| c.status)
        } else {
            None
        }
    }

    /// Загрузить клетку с WASM данными
    pub fn load_cell(&mut self, cell_id: usize, wasm_data: &[u8]) -> Result<(), WasmError> {
        if cell_id >= CELL_COUNT {
            return Err(WasmError::InstantiationError(String::from("Invalid cell ID")));
        }

        let module = Box::new(SimpleWasmModule::new());
        let mut instance = ModuleInstance::new(cell_id, module);
        instance.initialize(wasm_data)?;
        
        self.cells[cell_id] = Some(instance);
        Ok(())
    }

    /// Основная процедура загрузки системы
    /// 
    /// Порядок загрузки:
    /// 1. Cell 6 (ядро-спасатель) - инициализируется первым
    /// 2. Если Cell 6 в статусе Running, загружаем Cell 0 (главное ядро)
    /// 3. Остальные клетки загружаются по мере необходимости
    pub fn bootstrap(&mut self) -> Result<(), BootError> {
        // Вшиваемые WASM бинарники
        let init_wasm = include_bytes!("../wasm_bins/init.wasm");
        let emergency_wasm = include_bytes!("../wasm_bins/emergency.wasm");

        // Шаг 1: Инициализация ядра-спасателя (Cell 6)
        self.load_cell(RESCUE_CELL_ID, emergency_wasm)
            .map_err(|e| BootError::CellInitError(RESCUE_CELL_ID, e))?;

        // Проверка: Cell 6 должен быть в статусе Running
        if self.get_cell_status(RESCUE_CELL_ID) != Some(CellStatus::Running) {
            return Err(BootError::RescueCellNotRunning);
        }

        // Шаг 2: Только если Cell 6 Running, загружаем главное ядро (Cell 0)
        self.load_cell(MAIN_CELL_ID, init_wasm)
            .map_err(|e| BootError::CellInitError(MAIN_CELL_ID, e))?;

        Ok(())
    }

    /// Обработчик ошибок с логикой email-уведомлений
    /// 
    /// Если Cell 0 вылетает с WasmError:
    /// - Отправить EmailMessage в Cell 6 с телом 'CRASH_REPORT'
    /// - Ждать команду на восстановление
    pub fn handle_cell_error(&mut self, cell_id: usize, error: WasmError) {
        // Обновляем статус клетки
        if let Some(ref mut cell) = self.cells[cell_id] {
            cell.crash(error.clone());
        }

        // Делаем снимок состояния
        self.snapshot_take(cell_id);

        // Специальная логика для Cell 0
        if cell_id == MAIN_CELL_ID {
            match error {
                WasmError::Trap(_) | WasmError::InstantiationError(_) | 
                WasmError::CompileError(_) | WasmError::HostError(_) => {
                    // Формируем отчет об ошибке
                    let error_details = format!("{:?}", error);
                    
                    // Создаем email сообщение для Cell 6
                    let crash_report = EmailMessage::new(
                        MAIN_CELL_ID,           // from
                        RESCUE_CELL_ID,         // to  
                        "SYSTEM_CRASH",         // subject
                        "CRASH_REPORT",         // body
                    )
                    .high_priority();

                    // Добавляем детали ошибки в тело сообщения
                    // В реальной системе это может быть отдельным полем
                    self.send_email(crash_report);

                    // Переходим в режим ожидания команды от Cell 6
                    // Система теперь ждет указаний от ядра-спасателя
                }
                _ => {}
            }
        }
    }

    /// Получить команду восстановления от Cell 6
    pub fn check_recovery_command(&self) -> Option<RecoveryCommand> {
        // Проверяем почту от Cell 6
        for msg in &self.mail_queue {
            if msg.from == RESCUE_CELL_ID && msg.to == MAIN_CELL_ID {
                if msg.body.contains("RECOVER") {
                    return Some(RecoveryCommand::RestoreFromSnapshot);
                } else if msg.body.contains("RESTART") {
                    return Some(RecoveryCommand::FullRestart);
                } else if msg.body.contains("HALT") {
                    return Some(RecoveryCommand::Halt);
                }
            }
        }
        None
    }

    /// Выполнить команду восстановления
    pub fn execute_recovery(&mut self, command: RecoveryCommand) -> Result<(), WasmError> {
        match command {
            RecoveryCommand::RestoreFromSnapshot => {
                self.snapshot_restore(MAIN_CELL_ID)?;
                // После восстановления можно перезапустить клетку
                if let Some(ref mut cell) = self.cells[MAIN_CELL_ID] {
                    cell.status = CellStatus::Running;
                }
                Ok(())
            }
            RecoveryCommand::FullRestart => {
                // Полная перезагрузка главного ядра
                let init_wasm = include_bytes!("../wasm_bins/init.wasm");
                self.load_cell(MAIN_CELL_ID, init_wasm)
            }
            RecoveryCommand::Halt => {
                // Остановка системы
                if let Some(ref mut cell) = self.cells[MAIN_CELL_ID] {
                    cell.status = CellStatus::Stopped;
                }
                Ok(())
            }
        }
    }
}

/// Команды восстановления от ядра-спасателя
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryCommand {
    /// Восстановить из последнего снимка
    RestoreFromSnapshot,
    /// Полная перезагрузка
    FullRestart,
    /// Остановка
    Halt,
}

/// Ошибки загрузки
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootError {
    /// Ошибка инициализации клетки
    CellInitError(usize, WasmError),
    /// Ядро-спасатель не перешло в статус Running
    RescueCellNotRunning,
    /// Неверная конфигурация
    InvalidConfig,
}

// ============================================================================
// Global Instance (для использования в no_std среде)
// ============================================================================

thread_local! {
    static SCRAPBOOT_MANAGER: RefCell<Option<ScrapBootManager>> = RefCell::new(None);
}

/// Инициализировать глобальный менеджер
pub fn global_init() {
    SCRAPBOOT_MANAGER.with(|mgr| {
        let mut manager = ScrapBootManager::new();
        manager.init();
        *mgr.borrow_mut() = Some(manager);
    });
}

/// Получить доступ к глобальному менеджеру
pub fn with_manager<F, R>(f: F) -> R
where
    F: FnOnce(&mut ScrapBootManager) -> R,
{
    SCRAPBOOT_MANAGER.with(|mgr| {
        let mut borrowed = mgr.borrow_mut();
        if borrowed.is_none() {
            let mut manager = ScrapBootManager::new();
            manager.init();
            *borrowed = Some(manager);
        }
        f(borrowed.as_mut().unwrap())
    })
}

// ============================================================================
// Tests (доступны только в std режиме для тестирования)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bootstrap_sequence() {
        let mut manager = ScrapBootManager::new();
        manager.init();

        // Bootstrap должен сначала загрузить Cell 6
        let result = manager.bootstrap();
        assert!(result.is_ok());

        // Cell 6 должен быть Running
        assert_eq!(manager.get_cell_status(RESCUE_CELL_ID), Some(CellStatus::Running));
        
        // Cell 0 должен быть загружен только после успешной загрузки Cell 6
        assert_eq!(manager.get_cell_status(MAIN_CELL_ID), Some(CellStatus::Running));
    }

    #[test]
    fn test_snapshot_system() {
        let mut manager = ScrapBootManager::new();
        manager.init();

        // Загружаем тестовую клетку
        let test_wasm = b"test_wasm_data";
        manager.load_cell(1, test_wasm).unwrap();

        // Симулируем краш
        manager.handle_cell_error(1, WasmError::Trap(String::from("Test trap")));

        // Проверяем что снимок создан
        assert!(manager.snapshots.contains_key(&1));
    }

    #[test]
    fn test_email_on_crash() {
        let mut manager = ScrapBootManager::new();
        manager.init();
        manager.bootstrap().unwrap();

        // Симулируем краш Cell 0 с WasmError
        manager.handle_cell_error(MAIN_CELL_ID, WasmError::Trap(String::from("Critical error")));

        // Проверяем что письмо отправлено в Cell 6
        let mail_queue = manager.get_mail_queue();
        assert!(!mail_queue.is_empty());
        
        let crash_email = &mail_queue[0];
        assert_eq!(crash_email.from, MAIN_CELL_ID);
        assert_eq!(crash_email.to, RESCUE_CELL_ID);
        assert_eq!(crash_email.body, "CRASH_REPORT");
        assert!(crash_email.high_priority);
    }
}

// ============================================================================
// Allocator для no_std
// ============================================================================

#[cfg(not(test))]
use core::alloc::{GlobalAlloc, Layout};

#[cfg(not(test))]
struct DummyAllocator;

#[cfg(not(test))]
unsafe impl GlobalAlloc for DummyAllocator {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        // В реальной системе здесь будет вызов к системному аллокатору
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // NOP
    }
}

#[cfg(not(test))]
#[global_allocator]
static ALLOCATOR: DummyAllocator = DummyAllocator;

#[cfg(not(test))]
#[alloc_error_handler]
fn alloc_error_handler(_layout: Layout) -> ! {
    loop {}
}
