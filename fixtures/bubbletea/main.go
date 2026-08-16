package main

import (
	"fmt"
	"time"

	tea "github.com/charmbracelet/bubbletea"
)

type tickMsg struct{}
type model struct{ message string }

func (m model) Init() tea.Cmd {
	return tea.Tick(50*time.Millisecond, func(time.Time) tea.Msg { return tickMsg{} })
}

func (m model) Update(message tea.Msg) (tea.Model, tea.Cmd) {
	switch message := message.(type) {
	case tickMsg:
		m.message = "Bubble Tea async ready"
	case tea.KeyMsg:
		if message.String() == "q" || message.String() == "ctrl+c" {
			return m, tea.Quit
		}
		m.message = fmt.Sprintf("Bubble Tea key: %s", message.String())
	}
	return m, nil
}

func (m model) View() string { return m.message + "\n" }

func main() {
	if _, err := tea.NewProgram(model{message: "Bubble Tea loading"}).Run(); err != nil {
		panic(err)
	}
}
