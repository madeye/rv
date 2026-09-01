use gpui::actions;

actions!(
    rv,
    [
        NewConnection,
        ConnectSelected,
        DuplicateSelected,
        DeleteSelected,
        OpenProperties,
        OpenPreferences,
        ToggleViewMode,
        ToggleSidebar,
        FocusSearch,
        CloseModal,
        QuitApp,
        SessionFullscreen,
        SessionScaleCycle,
        SessionCad,
        SessionDisconnect,
        SessionToggleToolbar,
        SessionMenu,
        SessionClose,
    ]
);
