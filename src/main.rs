use std::ops::Sub;

use gpui::{
    Animation, AnimationExt, App, AsyncApp, BackgroundExecutor, Bounds, Div, FontWeight, Hsla,
    KeyBinding, Pixels, Point, Render, Task, TitlebarOptions, WeakEntity, Window, WindowBounds,
    WindowOptions, actions, canvas, div, prelude::*, px, rgb,
};
use palette::IntoColor;
use pastey::paste;

/// A macro for converting the # in a hex color to 0x
macro_rules! hex_color {
    (#$ns:tt) => {
        paste! {
            [<0 x $ns:upper>]
        }
    };
}

/// Amount of numbers kept in the history
const HISTORY_LENGTH: usize = 10;

/// Switch to turn the demotion timer on or off
const DEMOTION_TIMER_ACTIVE: bool = false;
/// The duration of how long a current number stays active, in milliseconds
const DEMOTION_TIMEOUT: u64 = 5000;

/// Colors
const BACKGROUND_COLOR: u32 = hex_color!(#171717);
const DIVIDER_COLOR: u32 = hex_color!(#e2e8f0);
const CURRENT_NUMBER_COLOR: u32 = hex_color!(#e2e8f0);
const HISTORY_COLOR: u32 = hex_color!(#e2e8f0);

/// Whether to lighten the history numbers linearly
const HISTORY_COLOR_METHOD: HistoryColorMethod = HistoryColorMethod::Opacity;

/// Font Sizes
const CURRENT_NUMBER_SIZE: Pixels = px(96.0);
const HISTORY_NUMBERS_SIZE_MAX: Pixels = px(96.0);

/// Layout Settings
const HISTORY_ENTRY_GAP: Pixels = px(32.0);

/// The type of number which the app represents
type NumberType = usize;

/// The embedded font (Nabla)
const UI_FONT: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/Nabla.ttf"));
const UI_FONT_NAME: &str = "Nabla";

/// The enum defining the History coloring method
#[allow(dead_code)]
enum HistoryColorMethod {
    /// Do nothing (color is unified)
    None,
    /// Lightens/Darkens by setting the Hsla value
    Hsla,
    /// Fades to transparent by setting the opacity
    Opacity,
}

/// The enum containing known messages
enum Message {
    /// A new number was entered
    NewNumber(NumberType),
    /// The sepcified timeout passed and the current number should be demoted
    /// to history
    DemoteToHistory,
}

/// The renderable struct
struct StatusDisplay {
    /// An array stored on the stack, holding HISTORY_LENGTH items
    history: [Option<NumberType>; HISTORY_LENGTH],
    /// The current number, if any
    current_number: Option<NumberType>,
    /// The current animation index for the current number
    current_number_anim_id: (&'static str, usize),
    /// Tasks associated with this instance, stored until the instance is
    /// dropped. Automatically cancelled on Drop.
    _tasks: [Task<()>; 3],
}

impl StatusDisplay {
    /// Creates a new StatusDisplay, spawning the two background polling
    /// threads and the foreground updater polling thread
    fn new(cx: &mut Context<Self>) -> Self {
        let (sender, receiver) = async_channel::unbounded::<Message>();
        let bgt1 = cx.background_spawn(Self::cli_polling_loop(sender.clone()));
        let bgt2 = cx.background_spawn(Self::demotion_timer_loop(
            cx.background_executor().clone(),
            sender.clone(),
        ));
        let fgt = cx.spawn(async |this, cx| Self::updater_loop(this, cx, receiver).await);
        Self {
            history: [None; HISTORY_LENGTH],
            current_number: None,
            current_number_anim_id: ("current_number_anim", 0usize),
            _tasks: [bgt1, bgt2, fgt],
        }
    }

    /// Checks if a number is currently active and - if so - demotes this
    /// number to the first entry of the history. All other entries in the
    /// history get shifted to the right by 1, causing the last history entry
    /// to be evicted. The current number is set to None after demotion.
    fn demote_current_to_history(&mut self) {
        if let Some(current_number) = self.current_number {
            self.history.rotate_right(1);
            self.history[0] = Some(current_number);
            self.current_number = None;
        }
    }

    /// The function performing the CLI number polling. It continuously waits
    /// for the user to input a whole number into the CLI and to confirm it
    /// with Enter. It then parses this number and - if parsing was successful
    /// - sends the number to the foreground thread to update the UI.
    async fn cli_polling_loop(sender: async_channel::Sender<Message>) {
        let mut stdin_buffer = String::new();
        loop {
            stdin_buffer.clear();
            eprintln!("Bitte eine Ganzzahl eingeben und mit Enter bestätigen:");
            std::io::stdin()
                .read_line(&mut stdin_buffer)
                .expect("Es ist ein Fehler beim Lese von STDIN aufgetreten!");
            let trimmed_stdin = stdin_buffer.trim_end();
            if let Ok(parsed_number) = trimmed_stdin.parse::<NumberType>() {
                if let Err(error) = sender.send(Message::NewNumber(parsed_number)).await {
                    eprintln!(
                        "Die eingegebene Zahl konnte nicht an den Hauptthread gesendet werden ({})! Fenster bitte neu öffnen.",
                        error
                    );
                    break;
                }
            } else {
                eprintln!(
                    "Die eingegebene Zahl konnte nicht in eine Zahl umgewandelt werden ({}), bitte erneut versuchen!",
                    trimmed_stdin
                );
            }
        }
    }

    /// The function performing the demotion timer. It sends the demotion
    /// message after exactly DEMOTION_TIMEOUT milliseconds.
    /// If DEMOTION_TIMER_ACTIVE is false, the loop is never started so
    /// automatic demotion never happens.
    async fn demotion_timer_loop(
        background_executor: BackgroundExecutor,
        sender: async_channel::Sender<Message>,
    ) {
        if DEMOTION_TIMER_ACTIVE {
            loop {
                background_executor
                    .timer(std::time::Duration::from_millis(DEMOTION_TIMEOUT))
                    .await;
                if let Err(error) = sender.send(Message::DemoteToHistory).await {
                    eprintln!(
                        "Die Nachricht zum Degradieren der aktuellen Zahl konnte nicht an den Hauptthread gesendet werden ({})! Fenster bitte erneut öffnen.",
                        error
                    );
                    break;
                }
            }
        }
    }

    /// The function performing the foreground UI updating. It waits for a
    /// message to be received and handles it accordingly:
    ///
    /// - Message::NewNumber: The currently active number - if any - is demoted
    ///   to history and replaced by the new number received.
    ///
    /// - Message::DemoteToHistory: The currently active number - if any - is
    ///   demoted to history, leaving an empty value (None) in its place.
    ///
    /// Work in this function should be as minimal as possible, because it
    /// blocks the Main Thread. Therefore, the work performed is restricted to
    /// updating the current UI state.
    async fn updater_loop(
        this: WeakEntity<Self>,
        cx: &mut AsyncApp,
        receiver: async_channel::Receiver<Message>,
    ) {
        while let Ok(message) = receiver.recv().await {
            let update_result = match message {
                Message::NewNumber(number) => this.update(cx, |this, cx| {
                    this.demote_current_to_history();
                    this.current_number = Some(number);
                    this.current_number_anim_id.1 += 1;
                    cx.notify();
                }),
                Message::DemoteToHistory => this.update(cx, |this, cx| {
                    this.demote_current_to_history();
                    cx.notify();
                }),
            };
            if let Err(error) = update_result {
                eprintln!(
                    "Der Hauptthread konnte die empfangene Nachricht nicht im UI aktualisieren, bitte erneut versuchen ({})!",
                    error
                )
            }
        }
    }

    /// Performs adaptive pixel scaling.
    /// Given a base pixel size, it calculates the current width ratio of the
    /// window and scales down the pixels accordingly, if the window has a
    /// smaller width than what the display allows for.
    fn adaptive_font_size(
        window: &mut Window,
        cx: &mut Context<Self>,
        base_pixels: Pixels,
    ) -> Pixels {
        let max_width = cx.primary_display().unwrap().bounds().size.width;
        let current_width = window.bounds().size.width;
        let scale_factor = current_width / max_width;
        base_pixels * scale_factor
    }

    /// Applies style refinements based on the current compile-time settings.
    /// Depending on HISTORY_COLOR_METHOD, different approaches are taken:
    ///
    /// - HistoryColorMethod::None: The color is unified for all list entries,
    ///   i.e., no darkening/lightning is applied.
    ///
    /// - HistoryColorMethod::Hsla: The color is darkened/lightened by de- or
    ///   increasing the lightness component of the color when converted to
    ///   Hsla.
    ///
    /// - HistoryColorMethod::Opacity: The color is applied uniformly for all
    ///   list entries, but the opacity of the whole container element is de-
    ///   or increased.
    fn perform_history_coloring(index: usize, element: Div) -> Div {
        match HISTORY_COLOR_METHOD {
            HistoryColorMethod::None => element.text_color(rgb(HISTORY_COLOR)),
            HistoryColorMethod::Hsla => {
                let mut hsla: Hsla = rgb(HISTORY_COLOR).into_color();
                if hsla.lightness >= 0.5 {
                    hsla.lightness = (hsla.lightness
                        - ((index as f32 / HISTORY_LENGTH as f32) / 2.0))
                        .clamp(0.0, 1.0);
                } else {
                    hsla.lightness = (hsla.lightness
                        + ((index as f32 / HISTORY_LENGTH as f32) / 2.0))
                        .clamp(0.0, 1.0);
                }
                element.text_color(hsla)
            }
            HistoryColorMethod::Opacity => {
                let opacity_value =
                    (1.0 - ((index as f32 / HISTORY_LENGTH as f32) / 2.0)).clamp(0.0, 1.0);
                element
                    .text_color(rgb(HISTORY_COLOR))
                    .opacity(opacity_value)
            }
        }
    }
}

/// The Render implementation for the StatusDisplay. It represents a split
/// view, the right view being the currently active number, the left view being
/// the history of numbers.
impl Render for StatusDisplay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let adaptive_size_current_number =
            Self::adaptive_font_size(window, cx, CURRENT_NUMBER_SIZE);
        let current_history_element_count = self
            .history
            .iter()
            .filter(|element| element.is_some())
            .count();
        let current_history = self.history;

        div()
            .w_full()
            .h_full()
            .pt_8()
            .pb_8()
            .bg(rgb(BACKGROUND_COLOR))
            .flex()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .items_center()
                    .h_full()
                    .flex_1()
                    .gap_8()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .items_end()
                            .text_color(rgb(CURRENT_NUMBER_COLOR))
                            .font_weight(FontWeight::LIGHT)
                            .child("Now starting:"),
                    )
                    .child(
                        div()
                            .w_full()
                            .h(adaptive_size_current_number)
                            .text_color(rgb(CURRENT_NUMBER_COLOR))
                            .font_family(UI_FONT_NAME)
                            .font_weight(FontWeight::BLACK)
                            .flex()
                            .justify_center()
                            .items_center()
                            .with_animation(
                                self.current_number_anim_id,
                                Animation::new(std::time::Duration::from_millis(500)),
                                move |this, factor: f32| {
                                    this.text_size(adaptive_size_current_number * factor)
                                },
                            )
                            .when_some(self.current_number, |this, number| {
                                this.child(format!("{}", number))
                            })
                            .when_none(&self.current_number, |this| this.child("")),
                    ),
            )
            .child(div().h_full().w_0p5().bg(rgb(DIVIDER_COLOR)).block())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .h_full()
                    .flex_1()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .items_end()
                            .text_color(rgb(CURRENT_NUMBER_COLOR))
                            .font_weight(FontWeight::LIGHT)
                            .child("Recently played:"),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex_1()
                            .font_weight(FontWeight::BOLD)
                            .font_family(UI_FONT_NAME)
                            .flex()
                            .flex_col()
                            .justify_center()
                            .items_center()
                            .child(
                                canvas(
                                    move |bounds, window, cx| {
                                        let available_height = bounds.size.height;
                                        let elements_to_be_painted =
                                            current_history_element_count.max(1) as f32;
                                        let maximum_text_size = (available_height
                                            / elements_to_be_painted)
                                            .floor()
                                            .min(HISTORY_NUMBERS_SIZE_MAX)
                                            .sub(HISTORY_ENTRY_GAP);
                                        let mut element = div()
                                            .w(bounds.size.width)
                                            .h(bounds.size.height)
                                            .flex()
                                            .flex_col()
                                            .items_center()
                                            .gap(HISTORY_ENTRY_GAP)
                                            .children(
                                                current_history.iter().flatten().enumerate().map(
                                                    |(index, history_entry)| {
                                                        Self::perform_history_coloring(
                                                            index,
                                                            div()
                                                                .h(maximum_text_size)
                                                                .line_height(maximum_text_size)
                                                                .text_size(maximum_text_size * 0.75)
                                                                .child(format!("{history_entry}")),
                                                        )
                                                    },
                                                ),
                                            )
                                            .into_any_element();
                                        element.prepaint_as_root(
                                            bounds.origin,
                                            bounds.size.into(),
                                            window,
                                            cx,
                                        );
                                        element
                                    },
                                    |_, mut element, window, cx| {
                                        element.paint(window, cx);
                                    },
                                )
                                .size_full(),
                            ),
                    ),
            )
    }
}

// Global Actions the application handles
actions!([Quit]);

/// The keystroke (OS-dependent) for quitting the application
const QUIT_KEYSTROKE: &str = cfg_select! {
    target_os = "macos" => "cmd-q",
    _ => "alt-f4"
};

/// The callback function for when the application finished launching. It
/// calculates the size of the primary display, creates the WindowOptions
/// accordingly and creates a new window with a StatusDisplay as its root view.
/// Finally activates the app, bringing it to the foreground.
fn app_did_finish_launching(cx: &mut App) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(UI_FONT)])
        .unwrap();
    let display_bounds = cx.primary_display().unwrap().visible_bounds();
    let window_bounds = WindowBounds::Windowed(Bounds::new(
        Point {
            x: px(0.0),
            y: px(0.0),
        },
        display_bounds.size,
    ));
    let titlebar_options = TitlebarOptions {
        appears_transparent: true,
        ..Default::default()
    };
    let window_options = WindowOptions {
        window_bounds: Some(window_bounds),
        titlebar: Some(titlebar_options),
        ..Default::default()
    };
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.bind_keys([KeyBinding::new(QUIT_KEYSTROKE, Quit, None)]);
    cx.open_window(window_options, |_, cx| cx.new(StatusDisplay::new))
        .unwrap();
    cx.activate(true);
}

/// The main function. Sets up the GPUI application and runs it.
fn main() {
    gpui_platform::application().run(app_did_finish_launching);
}
